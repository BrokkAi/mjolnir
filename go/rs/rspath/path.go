// Port of std 1.99.1 src/path.rs

package rspath

import (
	"iter"
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

////////////////////////////////////////////////////////////////////////////////
// GENERAL NOTES
////////////////////////////////////////////////////////////////////////////////
//
// A Rust Path, PathBuf, OsStr and OsString are all a Go string here: the
// encoded bytes of the OsStr, which on Unix are any bytes and on Windows are
// WTF-8. Rust's methods are functions taking the path first; the ones that
// change a PathBuf take a *string.
//
// Parsing in this module is done on those bytes directly, taking advantage of
// the fact that OsStr always encodes ASCII characters as-is.

////////////////////////////////////////////////////////////////////////////////
// Windows Prefixes
////////////////////////////////////////////////////////////////////////////////

// Prefix is std::path::Prefix: a Windows path prefix, e.g. `C:` or
// `\\server\share`.
//
// Windows uses a variety of path prefix styles, including references to drive
// volumes (like `C:`), network shared folders (like `\\server\share`), and
// others. In addition, some path prefixes are "verbatim" (i.e., prefixed with
// `\\?\`), in which case `/` is *not* treated as a separator and essentially
// no normalization is performed.
type Prefix interface {
	isPrefix()

	// IsVerbatim is Prefix::is_verbatim: whether this is a verbatim prefix,
	// i.e. one that begins `\\?\`.
	IsVerbatim() bool
}

// Prefix_Verbatim is the verbatim prefix `\\?\cat_pics`: `\\?\` followed by
// the given component.
type Prefix_Verbatim struct{ F0 string }

// Prefix_VerbatimUNC is the verbatim prefix `\\?\UNC\server\share` using
// Windows' Uniform Naming Convention: `\\?\UNC\`, then the server's hostname
// and a share name.
type Prefix_VerbatimUNC struct {
	F0 string
	F1 string
}

// Prefix_VerbatimDisk is the verbatim disk prefix `\\?\C:`: `\\?\` followed
// by the drive letter and `:`.
type Prefix_VerbatimDisk struct{ F0 uint8 }

// Prefix_DeviceNS is the device namespace prefix `\\.\COM42`: `\\.\`
// followed by the device name.
type Prefix_DeviceNS struct{ F0 string }

// Prefix_UNC is the prefix `\\server\share` using Windows' Uniform Naming
// Convention: `\\`, then the server's hostname and a share name.
type Prefix_UNC struct {
	F0 string
	F1 string
}

// Prefix_Disk is the prefix `C:` for the given disk drive.
type Prefix_Disk struct{ F0 uint8 }

func (*Prefix_Verbatim) isPrefix()     {}
func (*Prefix_VerbatimUNC) isPrefix()  {}
func (*Prefix_VerbatimDisk) isPrefix() {}
func (*Prefix_DeviceNS) isPrefix()     {}
func (*Prefix_UNC) isPrefix()          {}
func (*Prefix_Disk) isPrefix()         {}

// IsVerbatim is Prefix::is_verbatim.
func (*Prefix_Verbatim) IsVerbatim() bool { return true }

// IsVerbatim is Prefix::is_verbatim.
func (*Prefix_VerbatimUNC) IsVerbatim() bool { return true }

// IsVerbatim is Prefix::is_verbatim.
func (*Prefix_VerbatimDisk) IsVerbatim() bool { return true }

// IsVerbatim is Prefix::is_verbatim.
func (*Prefix_DeviceNS) IsVerbatim() bool { return false }

// IsVerbatim is Prefix::is_verbatim.
func (*Prefix_UNC) IsVerbatim() bool { return false }

// IsVerbatim is Prefix::is_verbatim.
func (*Prefix_Disk) IsVerbatim() bool { return false }

var _ = rs.RegisterEnum[Prefix]("Prefix",
	rs.Variant[Prefix_Verbatim]("Verbatim", rs.ShapeTuple),
	rs.Variant[Prefix_VerbatimUNC]("VerbatimUNC", rs.ShapeTuple),
	rs.Variant[Prefix_VerbatimDisk]("VerbatimDisk", rs.ShapeTuple),
	rs.Variant[Prefix_DeviceNS]("DeviceNS", rs.ShapeTuple),
	rs.Variant[Prefix_UNC]("UNC", rs.ShapeTuple),
	rs.Variant[Prefix_Disk]("Disk", rs.ShapeTuple),
)

// prefixLen is Prefix::len: how many bytes of the path the prefix takes.
func prefixLen(p Prefix) int {
	osStrLen := func(s string) int {
		return len(s)
	}
	switch p := p.(type) {
	case *Prefix_Verbatim:
		return 4 + osStrLen(p.F0)
	case *Prefix_VerbatimUNC:
		n := 8 + osStrLen(p.F0)
		if osStrLen(p.F1) > 0 {
			n += 1 + osStrLen(p.F1)
		}
		return n
	case *Prefix_VerbatimDisk:
		return 6
	case *Prefix_UNC:
		n := 2 + osStrLen(p.F0)
		if osStrLen(p.F1) > 0 {
			n += 1 + osStrLen(p.F1)
		}
		return n
	case *Prefix_DeviceNS:
		return 4 + osStrLen(p.F0)
	case *Prefix_Disk:
		return 2
	default:
		panic(rs.Unreachable(p))
	}
}

// prefixIsDrive is Prefix::is_drive.
func prefixIsDrive(p Prefix) bool {
	return rs.Is[*Prefix_Disk](p)
}

// prefixHasImplicitRoot is Prefix::has_implicit_root.
func prefixHasImplicitRoot(p Prefix) bool {
	return !prefixIsDrive(p)
}

// prefixIndex is the position of a variant in the enum, which is what a
// derived Ord compares first.
func prefixIndex(p Prefix) int {
	switch p.(type) {
	case *Prefix_Verbatim:
		return 0
	case *Prefix_VerbatimUNC:
		return 1
	case *Prefix_VerbatimDisk:
		return 2
	case *Prefix_DeviceNS:
		return 3
	case *Prefix_UNC:
		return 4
	case *Prefix_Disk:
		return 5
	default:
		panic(rs.Unreachable(p))
	}
}

// prefixCmp is the derived Ord of Prefix: by variant, then by fields.
func prefixCmp(a, b Prefix) int {
	if order := prefixIndex(a) - prefixIndex(b); order != 0 {
		return sign(order)
	}
	switch a := a.(type) {
	case *Prefix_Verbatim:
		return strings.Compare(a.F0, b.(*Prefix_Verbatim).F0)
	case *Prefix_VerbatimUNC:
		b := b.(*Prefix_VerbatimUNC)
		if order := strings.Compare(a.F0, b.F0); order != 0 {
			return order
		}
		return strings.Compare(a.F1, b.F1)
	case *Prefix_VerbatimDisk:
		return sign(int(a.F0) - int(b.(*Prefix_VerbatimDisk).F0))
	case *Prefix_DeviceNS:
		return strings.Compare(a.F0, b.(*Prefix_DeviceNS).F0)
	case *Prefix_UNC:
		b := b.(*Prefix_UNC)
		if order := strings.Compare(a.F0, b.F0); order != 0 {
			return order
		}
		return strings.Compare(a.F1, b.F1)
	case *Prefix_Disk:
		return sign(int(a.F0) - int(b.(*Prefix_Disk).F0))
	default:
		panic(rs.Unreachable(a))
	}
}

func sign(order int) int {
	switch {
	case order < 0:
		return -1
	case order > 0:
		return 1
	default:
		return 0
	}
}

////////////////////////////////////////////////////////////////////////////////
// Misc helpers
////////////////////////////////////////////////////////////////////////////////

// Iterate through `iter` while it matches `prefix`; return `None` if `prefix`
// is not a prefix of `iter`, otherwise return `Some(iter_after_prefix)` giving
// `iter` after having exhausted `prefix`.
//
// reversed is `iter.rev()` and `prefix.rev()`: both are read from the back.
func iterAfter(iter, prefix PathComponents, reversed bool) (PathComponents, bool) {
	next := func(c *PathComponents) rs.Option[Component] {
		if reversed {
			return c.NextBack()
		}
		return c.Next()
	}
	for {
		iterNext := iter
		x, xSome := next(&iterNext).Get()
		y, ySome := next(&prefix).Get()
		switch {
		case xSome && ySome && componentEq(x, y):
		case xSome && ySome:
			return PathComponents{}, false
		case xSome && !ySome:
			return iter, true
		case !xSome && !ySome:
			return iter, true
		default:
			return PathComponents{}, false
		}
		iter = iterNext
	}
}

////////////////////////////////////////////////////////////////////////////////
// Cross-platform, iterator-independent parsing
////////////////////////////////////////////////////////////////////////////////

// hasPhysicalRoot says whether the path has a root separator after its
// prefix.
func (f Flavor) hasPhysicalRoot(s string, prefix rs.Option[Prefix]) bool {
	path := s
	if p, ok := prefix.Get(); ok {
		path = s[prefixLen(p):]
	}
	return len(path) != 0 && f.isSepByte(path[0])
}

// basic workhorse for splitting stem and extension
func rsplitFileAtDot(file string) (rs.Option[string], rs.Option[string]) {
	if file == ".." {
		return rs.Some(file), rs.None[string]()
	}

	// file.rsplitn(2, '.'): what follows the last dot, then what precedes it.
	var after, before rs.Option[string]
	if dot := strings.LastIndexByte(file, '.'); dot >= 0 {
		after = rs.Some(file[dot+1:])
		before = rs.Some(file[:dot])
	} else {
		after = rs.Some(file)
		before = rs.None[string]()
	}
	if value, ok := before.Get(); ok && value == "" {
		return rs.Some(file), rs.None[string]()
	}
	return before, after
}

func splitFileAtDot(file string) (string, rs.Option[string]) {
	slice := file
	if slice == ".." {
		return file, rs.None[string]()
	}

	// A file name is never empty, so slice[1:] is in range, as it is in Rust.
	i := strings.IndexByte(slice[1:], '.')
	if i < 0 {
		return file, rs.None[string]()
	}
	i++
	before := slice[:i]
	after := slice[i+1:]
	return before, rs.Some(after)
}

// Checks whether the path is empty or contains path separators.
func (f Flavor) validateExtension(extension string) {
	for i := 0; i < len(extension); i++ {
		if f.isSepByte(extension[i]) {
			rs.PanicAt(0, "extension cannot contain path separators: "+f.DebugString(extension))
		}
	}
}

////////////////////////////////////////////////////////////////////////////////
// The core iterators
////////////////////////////////////////////////////////////////////////////////

// Component parsing works by a double-ended state machine; the cursors at the
// front and back of the path each keep track of what parts of the path have
// been consumed so far.
//
// Going front to back, a path is made up of a prefix, a starting
// directory component, and a body (of normal components)
type state uint8

const (
	statePrefix   state = 0 // c:
	stateStartDir state = 1 // / or . or nothing
	stateBody     state = 2 // foo/bar/baz
	stateDone     state = 3
)

// PrefixComponent is std::path::PrefixComponent: a structure wrapping a
// Windows path prefix as well as its unparsed string representation.
//
// In addition to the parsed Prefix information returned by Kind,
// PrefixComponent also holds the raw and unparsed OsStr slice, returned by
// AsOsStr.
//
// It is comparable by its parsed prefix alone, as in Rust: `c:` and `C:` are
// equal.
type PrefixComponent struct {
	// The prefix as an unparsed `OsStr` slice.
	raw string `rs:"raw"`

	// The parsed prefix data.
	parsed Prefix `rs:"parsed"`
}

var _ = rs.RegisterStruct[PrefixComponent]("PrefixComponent")

// Kind is PrefixComponent::kind: the parsed prefix data.
func (p PrefixComponent) Kind() Prefix {
	return p.parsed
}

// AsOsStr is PrefixComponent::as_os_str: the raw OsStr slice for this prefix.
func (p PrefixComponent) AsOsStr() string {
	return p.raw
}

// Equal is PartialEq for PrefixComponent.
func (p PrefixComponent) Equal(other PrefixComponent) bool {
	return prefixCmp(p.parsed, other.parsed) == 0
}

// Compare is Ord for PrefixComponent.
func (p PrefixComponent) Compare(other PrefixComponent) int {
	return prefixCmp(p.parsed, other.parsed)
}

// Component is std::path::Component: a single component of a path.
//
// A Component roughly corresponds to a substring between path separators
// (`/` or `\`). It is what Components yields.
type Component interface {
	isComponent()

	// AsOsStr is Component::as_os_str: the underlying OsStr slice.
	//
	// The root directory is the main separator of the platform the program
	// is built for, as in Rust. A component produced under another flavour
	// is spelled by that flavour's ComponentAsOsStr, and Iter does that.
	AsOsStr() string
}

// Component_Prefix is a Windows path prefix, e.g. `C:` or `\\server\share`.
//
// There is a large variety of prefix types, see Prefix's documentation for
// more.
//
// Does not occur on Unix.
type Component_Prefix struct{ F0 PrefixComponent }

// Component_RootDir is the root directory component, which appears after any
// prefix and before anything else.
//
// It represents a separator that designates that a path starts from root.
type Component_RootDir struct{}

// Component_CurDir is a reference to the current directory, i.e. `.`.
type Component_CurDir struct{}

// Component_ParentDir is a reference to the parent directory, i.e. `..`.
type Component_ParentDir struct{}

// Component_Normal is a normal component, e.g. `a` and `b` in `a/b`.
//
// This variant is the most common one, it represents references to files
// or directories.
type Component_Normal struct{ F0 string }

func (*Component_Prefix) isComponent()    {}
func (*Component_RootDir) isComponent()   {}
func (*Component_CurDir) isComponent()    {}
func (*Component_ParentDir) isComponent() {}
func (*Component_Normal) isComponent()    {}

// AsOsStr is Component::as_os_str.
func (c *Component_Prefix) AsOsStr() string { return c.F0.AsOsStr() }

// AsOsStr is Component::as_os_str.
func (*Component_RootDir) AsOsStr() string { return MainSeparatorStr }

// AsOsStr is Component::as_os_str.
func (*Component_CurDir) AsOsStr() string { return "." }

// AsOsStr is Component::as_os_str.
func (*Component_ParentDir) AsOsStr() string { return ".." }

// AsOsStr is Component::as_os_str.
func (c *Component_Normal) AsOsStr() string { return c.F0 }

var _ = rs.RegisterEnum[Component]("Component",
	rs.Variant[Component_Prefix]("Prefix", rs.ShapeTuple),
	rs.Variant[Component_RootDir]("RootDir", rs.ShapeUnit),
	rs.Variant[Component_CurDir]("CurDir", rs.ShapeUnit),
	rs.Variant[Component_ParentDir]("ParentDir", rs.ShapeUnit),
	rs.Variant[Component_Normal]("Normal", rs.ShapeTuple),
)

// The data-free variants are shared: a Component is never changed in place.
var (
	componentRootDir   Component = &Component_RootDir{}
	componentCurDir    Component = &Component_CurDir{}
	componentParentDir Component = &Component_ParentDir{}
)

// ComponentAsOsStr is Component::as_os_str under this flavour: the root
// directory is this flavour's main separator.
func (f Flavor) ComponentAsOsStr(c Component) string {
	if rs.Is[*Component_RootDir](c) {
		return f.MainSeparatorStr()
	}
	return c.AsOsStr()
}

// componentIndex is the position of a variant in the enum, which is what a
// derived Ord compares first.
func componentIndex(c Component) int {
	switch c.(type) {
	case *Component_Prefix:
		return 0
	case *Component_RootDir:
		return 1
	case *Component_CurDir:
		return 2
	case *Component_ParentDir:
		return 3
	case *Component_Normal:
		return 4
	default:
		panic(rs.Unreachable(c))
	}
}

// componentCmp is the derived Ord of Component.
func componentCmp(a, b Component) int {
	if order := componentIndex(a) - componentIndex(b); order != 0 {
		return sign(order)
	}
	switch a := a.(type) {
	case *Component_Prefix:
		return a.F0.Compare(b.(*Component_Prefix).F0)
	case *Component_Normal:
		return strings.Compare(a.F0, b.(*Component_Normal).F0)
	default:
		return 0
	}
}

// componentEq is the derived PartialEq of Component.
func componentEq(a, b Component) bool {
	return componentCmp(a, b) == 0
}

// ComponentEq is `a == b` for two components, as Component's derived
// PartialEq compares them: by variant, a prefix by its parsed form, a normal
// component by its bytes. rs.Eq gives the same answer.
func ComponentEq(a, b Component) bool {
	return componentEq(a, b)
}

// ComponentCmp is `a.cmp(&b)` for two components, as Component's derived Ord
// orders them: Prefix, RootDir, CurDir, ParentDir, Normal, then by content.
// rs.Cmp gives the same answer.
func ComponentCmp(a, b Component) int {
	return componentCmp(a, b)
}

// PathComponents is std::path::Components: an iterator over the Components
// of a path. (The Go name differs because Components is the function, as
// Path::components is.)
//
// It is what the Components function returns. It is double-ended: Next takes
// from the front and NextBack from the back. Copying the struct is cloning
// the iterator.
type PathComponents struct {
	// The path left to parse components from
	path string

	// The prefix as it was originally parsed, if any
	prefix rs.Option[Prefix]

	// true if path *physically* has a root separator; for most Windows
	// prefixes, it may have a "logical" root separator for the purposes of
	// normalization, e.g., \\server\share == \\server\share\.
	hasPhysicalRoot bool

	// The iterator is double-ended, and these two states keep track of what has
	// been produced from either end
	front state
	back  state

	// The syntax the path is read in. Rust compiles one in.
	flavor Flavor
}

// PathIter is std::path::Iter: an iterator over the Components of a path, as
// OsStr slices.
//
// It is what the Iter function returns.
type PathIter struct {
	inner PathComponents
}

// componentsDebugHelper is the DebugHelper of `impl Debug for Components`.
type componentsDebugHelper struct {
	flavor Flavor
	path   string
}

func (h componentsDebugHelper) FmtDebug(f *rs.Formatter) error {
	var entries []componentDebug
	for component := range h.flavor.Components(h.path).All() {
		entries = append(entries, componentDebug{flavor: h.flavor, component: component})
	}
	return f.DebugList().Entries(entries).Finish()
}

// FmtDebug is `impl Debug for Components`.
func (c *PathComponents) FmtDebug(f *rs.Formatter) error {
	return f.DebugTuple("Components").Field(componentsDebugHelper{flavor: c.flavor, path: c.AsPath()}).Finish()
}

// DebugString is `format!("{:?}", components)`: the components still to
// come, as `Components([RootDir, Normal("tmp")])`.
func (c *PathComponents) DebugString() string {
	var b strings.Builder
	b.WriteString("Components([")
	for i, component := range c.flavor.Components(c.AsPath()).Collect() {
		if i > 0 {
			b.WriteString(", ")
		}
		b.WriteString(c.flavor.componentDebugString(component))
	}
	b.WriteString("])")
	return b.String()
}

// how long is the prefix, if any?
func (c *PathComponents) prefixLen() int {
	if !c.flavor.hasPrefixes() {
		return 0
	}
	if p, ok := c.prefix.Get(); ok {
		return prefixLen(p)
	}
	return 0
}

func (c *PathComponents) prefixVerbatim() bool {
	if !c.flavor.hasPrefixes() {
		return false
	}
	if p, ok := c.prefix.Get(); ok {
		return p.IsVerbatim()
	}
	return false
}

// how much of the prefix is left from the point of view of iteration?
func (c *PathComponents) prefixRemaining() int {
	if !c.flavor.hasPrefixes() {
		return 0
	}
	if c.front == statePrefix {
		return c.prefixLen()
	}
	return 0
}

// Given the iteration so far, how much of the pre-State::Body path is left?
func (c *PathComponents) lenBeforeBody() int {
	root := 0
	if c.front <= stateStartDir && c.hasPhysicalRoot {
		root = 1
	}
	curDir := 0
	if c.front <= stateStartDir && c.includeCurDir() {
		curDir = 1
	}
	return c.prefixRemaining() + root + curDir
}

// is the iteration complete?
func (c *PathComponents) finished() bool {
	return c.front == stateDone || c.back == stateDone || c.front > c.back
}

func (c *PathComponents) isSepByte(b byte) bool {
	if c.prefixVerbatim() {
		return c.flavor.isVerbatimSep(b)
	}
	return c.flavor.isSepByte(b)
}

// AsPath is Components::as_path: the path the remaining components stand
// for, as a slice of the original.
func (c *PathComponents) AsPath() string {
	comps := *c
	if comps.front == stateBody {
		comps.trimLeft()
	}
	if comps.back == stateBody {
		comps.trimRight()
	}
	return comps.path
}

// Is the *original* path rooted?
func (c *PathComponents) hasRoot() bool {
	if c.hasPhysicalRoot {
		return true
	}
	if c.flavor.hasPrefixes() {
		if p, ok := c.prefix.Get(); ok {
			if prefixHasImplicitRoot(p) {
				return true
			}
		}
	}
	return false
}

// Should the normalized path include a leading . ?
func (c *PathComponents) includeCurDir() bool {
	if c.hasRoot() {
		return false
	}
	slice := c.path[c.prefixRemaining():]
	switch {
	case len(slice) == 1 && slice[0] == '.':
		return true
	case len(slice) >= 2 && slice[0] == '.':
		return c.isSepByte(slice[1])
	default:
		return false
	}
}

// parse a given byte sequence following the OsStr encoding into the
// corresponding path component
func (c *PathComponents) parseSingleComponent(comp string) rs.Option[Component] {
	switch {
	case comp == "." && c.flavor.hasPrefixes() && c.prefixVerbatim():
		return rs.Some(componentCurDir)
	case comp == ".":
		// . components are normalized away, except at
		// the beginning of a path, which is treated
		// separately via `include_cur_dir`
		return rs.None[Component]()
	case comp == "..":
		return rs.Some(componentParentDir)
	case comp == "":
		return rs.None[Component]()
	default:
		return rs.Some[Component](&Component_Normal{F0: comp})
	}
}

// parse a component from the left, saying how many bytes to consume to
// remove the component
func (c *PathComponents) parseNextComponent() (int, rs.Option[Component]) {
	extra, comp := 0, c.path
	for i := 0; i < len(c.path); i++ {
		if c.isSepByte(c.path[i]) {
			extra, comp = 1, c.path[:i]
			break
		}
	}
	return len(comp) + extra, c.parseSingleComponent(comp)
}

// parse a component from the right, saying how many bytes to consume to
// remove the component
func (c *PathComponents) parseNextComponentBack() (int, rs.Option[Component]) {
	start := c.lenBeforeBody()
	extra, comp := 0, c.path[start:]
	for i := len(c.path) - 1; i >= start; i-- {
		if c.isSepByte(c.path[i]) {
			extra, comp = 1, c.path[i+1:]
			break
		}
	}
	return len(comp) + extra, c.parseSingleComponent(comp)
}

// trim away repeated separators (i.e., empty components) on the left
func (c *PathComponents) trimLeft() {
	for len(c.path) != 0 {
		size, comp := c.parseNextComponent()
		if comp.IsSome() {
			return
		}
		c.path = c.path[size:]
	}
}

// trim away repeated separators (i.e., empty components) on the right
func (c *PathComponents) trimRight() {
	for len(c.path) > c.lenBeforeBody() {
		size, comp := c.parseNextComponentBack()
		if comp.IsSome() {
			return
		}
		c.path = c.path[:len(c.path)-size]
	}
}

// FmtDebug is `impl Debug for Iter`.
func (i *PathIter) FmtDebug(f *rs.Formatter) error {
	return f.DebugTuple("Iter").Field(iterDebugHelper{flavor: i.inner.flavor, path: i.AsPath()}).Finish()
}

// iterDebugHelper is the DebugHelper of `impl Debug for Iter`.
type iterDebugHelper struct {
	flavor Flavor
	path   string
}

func (h iterDebugHelper) FmtDebug(f *rs.Formatter) error {
	var entries []PathDebug
	for _, part := range h.flavor.Iter(h.path).Collect() {
		entries = append(entries, h.flavor.Debug(part))
	}
	return f.DebugList().Entries(entries).Finish()
}

// DebugString is `format!("{:?}", iter)`: the parts still to come, as
// `Iter(["/", "tmp"])`.
func (i *PathIter) DebugString() string {
	flavor := i.inner.flavor
	var b strings.Builder
	b.WriteString("Iter([")
	for index, part := range flavor.Iter(i.AsPath()).Collect() {
		if index > 0 {
			b.WriteString(", ")
		}
		b.WriteString(flavor.DebugString(part))
	}
	b.WriteString("])")
	return b.String()
}

// AsPath is Iter::as_path: the path the remaining parts stand for, as a
// slice of the original.
func (i *PathIter) AsPath() string {
	return i.inner.AsPath()
}

// Next is Iterator::next for Iter.
func (i *PathIter) Next() rs.Option[string] {
	if c, ok := i.inner.Next().Get(); ok {
		return rs.Some(i.inner.flavor.ComponentAsOsStr(c))
	}
	return rs.None[string]()
}

// NextBack is DoubleEndedIterator::next_back for Iter.
func (i *PathIter) NextBack() rs.Option[string] {
	if c, ok := i.inner.NextBack().Get(); ok {
		return rs.Some(i.inner.flavor.ComponentAsOsStr(c))
	}
	return rs.None[string]()
}

// All ranges over the parts from the front, consuming the iterator as a Rust
// `for` loop does:
//
//	for part := range rspath.Iter(path).All() { ... }
func (i *PathIter) All() iter.Seq[string] {
	return func(yield func(string) bool) {
		for {
			part, ok := i.Next().Get()
			if !ok || !yield(part) {
				return
			}
		}
	}
}

// Rev ranges over the parts from the back: it is `iter.rev()`.
func (i *PathIter) Rev() iter.Seq[string] {
	return func(yield func(string) bool) {
		for {
			part, ok := i.NextBack().Get()
			if !ok || !yield(part) {
				return
			}
		}
	}
}

// Collect is `iter.collect::<Vec<_>>()`: the remaining parts, front to back.
func (i *PathIter) Collect() []string {
	var parts []string
	for part := range i.All() {
		parts = append(parts, part)
	}
	return parts
}

// Clone is `iter.clone()`.
func (i *PathIter) Clone() *PathIter {
	clone := *i
	return &clone
}

// Next is Iterator::next for Components.
func (c *PathComponents) Next() rs.Option[Component] {
	for !c.finished() {
		switch {
		// most likely case first
		case c.front == stateBody && len(c.path) != 0:
			size, comp := c.parseNextComponent()
			c.path = c.path[size:]
			if comp.IsSome() {
				return comp
			}
		case c.front == stateBody:
			c.front = stateDone
		case c.front == stateStartDir:
			c.front = stateBody
			if c.hasPhysicalRoot {
				c.path = c.path[1:]
				return rs.Some(componentRootDir)
			} else if p, ok := c.prefix.Get(); c.flavor.hasPrefixes() && ok {
				if prefixHasImplicitRoot(p) && !p.IsVerbatim() {
					return rs.Some(componentRootDir)
				}
			} else if c.includeCurDir() {
				c.path = c.path[1:]
				return rs.Some(componentCurDir)
			}
		case !c.flavor.hasPrefixes():
			panic(rs.Unreachable())
		case c.front == statePrefix && c.prefixLen() == 0:
			c.front = stateStartDir
		case c.front == statePrefix:
			c.front = stateStartDir
			raw := c.path[:c.prefixLen()]
			c.path = c.path[c.prefixLen():]
			return rs.Some[Component](&Component_Prefix{F0: PrefixComponent{
				raw:    raw,
				parsed: c.prefix.Unwrap(),
			}})
		default:
			panic(rs.Unreachable())
		}
	}
	return rs.None[Component]()
}

// NextBack is DoubleEndedIterator::next_back for Components.
func (c *PathComponents) NextBack() rs.Option[Component] {
	comp, _ := c.nextBackEnd()
	return comp
}

// nextBackEnd is next_back, and also says how long the remaining path was
// before the returned component was taken off it: where the component ends
// in the original path when nothing has been taken from the front. Rust
// finds that by pointer arithmetic on the slices (set_extension,
// add_extension).
func (c *PathComponents) nextBackEnd() (rs.Option[Component], int) {
	for !c.finished() {
		switch {
		case c.back == stateBody && len(c.path) > c.lenBeforeBody():
			end := len(c.path)
			size, comp := c.parseNextComponentBack()
			c.path = c.path[:len(c.path)-size]
			if comp.IsSome() {
				return comp, end
			}
		case c.back == stateBody:
			c.back = stateStartDir
		case c.back == stateStartDir:
			if c.flavor.hasPrefixes() {
				c.back = statePrefix
			} else {
				c.back = stateDone
			}
			if c.hasPhysicalRoot {
				end := len(c.path)
				c.path = c.path[:len(c.path)-1]
				return rs.Some(componentRootDir), end
			} else if p, ok := c.prefix.Get(); c.flavor.hasPrefixes() && ok {
				if prefixHasImplicitRoot(p) && !p.IsVerbatim() {
					return rs.Some(componentRootDir), len(c.path)
				}
			} else if c.includeCurDir() {
				end := len(c.path)
				c.path = c.path[:len(c.path)-1]
				return rs.Some(componentCurDir), end
			}
		case !c.flavor.hasPrefixes():
			panic(rs.Unreachable())
		case c.back == statePrefix && c.prefixLen() > 0:
			c.back = stateDone
			return rs.Some[Component](&Component_Prefix{F0: PrefixComponent{
				raw:    c.path,
				parsed: c.prefix.Unwrap(),
			}}), len(c.path)
		case c.back == statePrefix:
			c.back = stateDone
			return rs.None[Component](), len(c.path)
		default:
			panic(rs.Unreachable())
		}
	}
	return rs.None[Component](), len(c.path)
}

// All ranges over the components from the front, consuming the iterator as a
// Rust `for` loop does:
//
//	for component := range rspath.Components(path).All() { ... }
func (c *PathComponents) All() iter.Seq[Component] {
	return func(yield func(Component) bool) {
		for {
			component, ok := c.Next().Get()
			if !ok || !yield(component) {
				return
			}
		}
	}
}

// Rev ranges over the components from the back: it is `components.rev()`.
func (c *PathComponents) Rev() iter.Seq[Component] {
	return func(yield func(Component) bool) {
		for {
			component, ok := c.NextBack().Get()
			if !ok || !yield(component) {
				return
			}
		}
	}
}

// Collect is `components.collect::<Vec<_>>()`: the remaining components,
// front to back.
func (c *PathComponents) Collect() []Component {
	var components []Component
	for component := range c.All() {
		components = append(components, component)
	}
	return components
}

// Count is `components.count()`: how many components remain. It consumes
// the iterator.
func (c *PathComponents) Count() int {
	count := 0
	for c.Next().IsSome() {
		count++
	}
	return count
}

// Clone is `components.clone()`.
func (c *PathComponents) Clone() *PathComponents {
	clone := *c
	return &clone
}

// Equal is PartialEq for Components: the components still to come are the
// same.
func (c *PathComponents) Equal(other *PathComponents) bool {
	// Fast path for exact matches, e.g. for hashmap lookups.
	// Don't explicitly compare the prefix or has_physical_root fields since they'll
	// either be covered by the `path` buffer or are only relevant for `prefix_verbatim()`.
	if len(c.path) == len(other.path) &&
		c.front == other.front &&
		c.back == stateBody &&
		other.back == stateBody &&
		c.prefixVerbatim() == other.prefixVerbatim() {
		// possible future improvement: this could bail out earlier if there were a
		// reverse memcmp/bcmp comparing back to front
		if c.path == other.path {
			return true
		}
	}

	// compare back to front since absolute paths often share long prefixes
	left, right := *c, *other
	for {
		x, xSome := left.NextBack().Get()
		if !xSome {
			return right.NextBack().IsNone()
		}
		y, ySome := right.NextBack().Get()
		if !ySome {
			return false
		}
		if !componentEq(x, y) {
			return false
		}
	}
}

// Compare is Ord for Components: the components still to come, compared one
// by one.
func (c *PathComponents) Compare(other *PathComponents) int {
	return compareComponents(*c, *other)
}

func compareComponents(left, right PathComponents) int {
	// Fast path for long shared prefixes
	//
	// - compare raw bytes to find first mismatch
	// - backtrack to find separator before mismatch to avoid ambiguous parsings of '.' or '..' characters
	// - if found update state to only do a component-wise comparison on the remainder,
	//   otherwise do it on the full path
	//
	// The fast path isn't taken for paths with a PrefixComponent to avoid backtracking into
	// the middle of one
	if left.prefix.IsNone() && right.prefix.IsNone() && left.front == right.front {
		// possible future improvement: a [u8]::first_mismatch simd implementation
		firstDifference := -1
		shared := min(len(left.path), len(right.path))
		for i := 0; i < shared; i++ {
			if left.path[i] != right.path[i] {
				firstDifference = i
				break
			}
		}
		if firstDifference < 0 {
			if len(left.path) == len(right.path) {
				return 0
			}
			firstDifference = shared
		}

		for previousSep := firstDifference - 1; previousSep >= 0; previousSep-- {
			if left.isSepByte(left.path[previousSep]) {
				mismatchedComponentStart := previousSep + 1
				left.path = left.path[mismatchedComponentStart:]
				left.front = stateBody
				right.path = right.path[mismatchedComponentStart:]
				right.front = stateBody
				break
			}
		}
	}

	// Iterator::cmp(left, right)
	for {
		x, xSome := left.Next().Get()
		if !xSome {
			if right.Next().IsNone() {
				return 0
			}
			return -1
		}
		y, ySome := right.Next().Get()
		if !ySome {
			return 1
		}
		if order := componentCmp(x, y); order != 0 {
			return order
		}
	}
}

// PathAncestors is std::path::Ancestors: an iterator over a path and its
// ancestors.
//
// It is what the Ancestors function returns.
type PathAncestors struct {
	next   rs.Option[string]
	flavor Flavor
}

// Next is Iterator::next for Ancestors.
func (a *PathAncestors) Next() rs.Option[string] {
	next := a.next
	if path, ok := next.Get(); ok {
		a.next = a.flavor.Parent(path)
	} else {
		a.next = rs.None[string]()
	}
	return next
}

// All ranges over the path and its ancestors, consuming the iterator:
//
//	for ancestor := range rspath.Ancestors(path).All() { ... }
func (a *PathAncestors) All() iter.Seq[string] {
	return func(yield func(string) bool) {
		for {
			ancestor, ok := a.Next().Get()
			if !ok || !yield(ancestor) {
				return
			}
		}
	}
}

// Collect is `ancestors.collect::<Vec<_>>()`.
func (a *PathAncestors) Collect() []string {
	var ancestors []string
	for ancestor := range a.All() {
		ancestors = append(ancestors, ancestor)
	}
	return ancestors
}

////////////////////////////////////////////////////////////////////////////////
// Basic types and traits
////////////////////////////////////////////////////////////////////////////////

// Push is PathBuf::push: it extends buf with path.
//
// If path is absolute, it replaces the current path.
//
// On Windows:
//
//   - if path has a root but no prefix (e.g., `\windows`), it
//     replaces everything except for the prefix (if any) of buf.
//   - if path has a prefix but no root, it replaces buf.
//   - if buf has a verbatim prefix (e.g. `\\?\C:\windows`)
//     and path is not empty, the new path is normalized: all references
//     to `.` and `..` are removed.
//
// Consider using Join if you need a new path instead of changing buf.
func (f Flavor) Push(buf *string, path string) {
	f = f.resolve()
	// in general, a separator is needed if the rightmost byte is not a separator
	needSep := len(*buf) != 0 && !f.isSepByte((*buf)[len(*buf)-1])

	// in the special case of `C:` on Windows, do *not* add a separator
	comps := f.components(*buf)

	if comps.prefixLen() > 0 &&
		comps.prefixLen() == len(comps.path) &&
		prefixIsDrive(comps.prefix.Unwrap()) {
		needSep = false
	}

	// On Unix: prefix is always None.
	needClear := f.IsAbsolute(path) || f.prefix(path).IsSome()

	// absolute `path` replaces `self`
	if needClear {
		*buf = ""

		// verbatim paths need . and .. removed
	} else if comps.prefixVerbatim() && len(path) != 0 {
		parts := comps.Collect()
		for c := range f.Components(path).All() {
			switch c.(type) {
			case *Component_RootDir:
				if len(parts) > 1 {
					parts = parts[:1]
				}
				parts = append(parts, c)
			case *Component_CurDir:
			case *Component_ParentDir:
				if len(parts) > 0 && rs.Is[*Component_Normal](parts[len(parts)-1]) {
					parts = parts[:len(parts)-1]
				}
			default:
				parts = append(parts, c)
			}
		}

		res := ""
		needSep := false

		for _, c := range parts {
			if needSep && !rs.Is[*Component_RootDir](c) {
				f.OsStringPush(&res, f.MainSeparatorStr())
			}
			f.OsStringPush(&res, f.ComponentAsOsStr(c))

			switch c := c.(type) {
			case *Component_RootDir:
				needSep = false
			case *Component_Prefix:
				needSep = !prefixIsDrive(c.F0.parsed) && prefixLen(c.F0.parsed) > 0
			default:
				needSep = true
			}
		}

		*buf = res
		return

		// `path` has a root but no prefix, e.g., `\windows` (Windows only)
	} else if f.HasRoot(path) {
		prefixLen := comps.prefixRemaining()
		*buf = (*buf)[:prefixLen]

		// `path` is a pure relative path
	} else if needSep {
		*buf += f.MainSeparatorStr()
	}

	f.OsStringPush(buf, path)
}

// Pop is PathBuf::pop: it truncates buf to its parent.
//
// Returns false and does nothing if Parent is None. Otherwise, returns true.
func (f Flavor) Pop(buf *string) bool {
	if parent, ok := f.Parent(*buf).Get(); ok {
		*buf = (*buf)[:len(parent)]
		return true
	}
	return false
}

// SetTrailingSep is PathBuf::set_trailing_sep (unstable in Rust 1.99.1): it
// sets whether the path has a trailing separator.
//
// The value returned by HasTrailingSep will be equivalent to the provided
// value if possible.
func (f Flavor) SetTrailingSep(buf *string, trailingSep bool) {
	if trailingSep {
		f.PushTrailingSep(buf)
	} else {
		f.PopTrailingSep(buf)
	}
}

// PushTrailingSep is PathBuf::push_trailing_sep (unstable in Rust 1.99.1):
// it adds a trailing separator to the path.
//
// This acts similarly to WithTrailingSep, but mutates the underlying
// PathBuf.
func (f Flavor) PushTrailingSep(buf *string) {
	if !f.HasTrailingSep(*buf) {
		f.Push(buf, "")
	}
}

// PopTrailingSep is PathBuf::pop_trailing_sep (unstable in Rust 1.99.1): it
// removes a trailing separator from the path, if possible.
//
// This acts similarly to TrimTrailingSep, but mutates the underlying
// PathBuf.
func (f Flavor) PopTrailingSep(buf *string) {
	*buf = (*buf)[:len(f.TrimTrailingSep(*buf))]
}

// SetFileName is PathBuf::set_file_name: it updates FileName(buf) to
// fileName.
//
// If FileName was None, this is equivalent to pushing fileName.
//
// Otherwise it is equivalent to calling Pop and then pushing fileName. The
// new path will be a sibling of the original path. (That is, it will have
// the same parent.)
//
// The argument is not sanitized, so can include separators. This behavior
// may be changed to a panic in the future.
func (f Flavor) SetFileName(buf *string, fileName string) {
	if f.FileName(*buf).IsSome() {
		f.Pop(buf)
	}
	f.Push(buf, fileName)
}

// SetExtension is PathBuf::set_extension: it updates Extension(buf) to
// Some(extension) or to None if extension is empty.
//
// Returns false and does nothing if FileName is None, returns true and
// updates the extension otherwise.
//
// If Extension is None, the extension is added; otherwise it is replaced.
//
// If extension is the empty string, Extension will be None afterwards, not
// Some("").
//
// It panics if the passed extension contains a path separator.
//
// The new extension may contain dots and will be used in its entirety, but
// only the part after the final dot will be reflected in Extension.
//
// If the file stem contains internal dots and extension is empty, part of
// the old file stem will be considered the new Extension.
func (f Flavor) SetExtension(buf *string, extension string) bool {
	f.validateExtension(extension)

	name, end, ok := f.fileNameEnd(*buf)
	if !ok {
		return false
	}
	before, after := rsplitFileAtDot(name)
	fileStem, ok := before.Or(after).Get()
	if !ok {
		return false
	}

	// truncate until right after the file stem
	//
	// The stem starts where the file name starts, so it ends its length
	// after that. Rust reads the same offset off the slices' addresses.
	endFileStem := end - len(name) + len(fileStem)
	*buf = (*buf)[:endFileStem]

	// add the new extension, if any
	if len(extension) != 0 {
		*buf += "." + extension
	}

	return true
}

// AddExtension is PathBuf::add_extension: it appends to Extension(buf).
//
// Returns false and does nothing if FileName is None, returns true and
// updates the extension otherwise.
//
// It panics if the passed extension contains a path separator.
//
// The appended extension may contain dots and will be used in its entirety,
// but only the part after the final dot will be reflected in Extension.
func (f Flavor) AddExtension(buf *string, extension string) bool {
	f.validateExtension(extension)

	_, end, ok := f.fileNameEnd(*buf)
	if !ok {
		return false
	}

	if len(extension) != 0 {
		// truncate until right after the file name
		// this is necessary for trimming the trailing separator
		*buf = (*buf)[:end]

		// append the new extension
		*buf += "." + extension
	}

	return true
}

// PathBufIntoString is PathBuf::into_string: the path as a String if it is
// valid Unicode. On failure the error is an *IntoStringError holding the
// path, as Rust hands the PathBuf back.
func (f Flavor) PathBufIntoString(path string) (string, error) {
	return f.OsStringIntoString(path)
}

// PathBufFromIter is `iter.collect::<PathBuf>()` and PathBuf::from_iter: an
// empty PathBuf with every part pushed in turn.
func (f Flavor) PathBufFromIter(parts ...string) string {
	buf := ""
	f.Extend(&buf, parts...)
	return buf
}

// Extend is `PathBuf::extend`: every part is pushed in turn.
func (f Flavor) Extend(buf *string, parts ...string) {
	for _, part := range parts {
		f.Push(buf, part)
	}
}

// PathBufFromComponents is `components.collect::<PathBuf>()`: an empty
// PathBuf with every component pushed in turn.
func (f Flavor) PathBufFromComponents(components []Component) string {
	buf := ""
	f.ExtendComponents(&buf, components)
	return buf
}

// ExtendComponents is PathBuf::extend over components: each is pushed in
// turn.
func (f Flavor) ExtendComponents(buf *string, components []Component) {
	for _, component := range components {
		f.Push(buf, f.ComponentAsOsStr(component))
	}
}

// StripPrefixError is std::path::StripPrefixError: the error StripPrefix
// returns if the prefix was not found.
type StripPrefixError struct{ F0 rs.Unit }

var _ = rs.RegisterTupleStruct[StripPrefixError]("StripPrefixError")

// String holds the Display text.
func (e *StripPrefixError) String() string { return "prefix not found" }

// Error makes StripPrefixError a Go error with its Display text.
func (e *StripPrefixError) Error() string { return e.String() }

// FmtDisplay is `impl Display for StripPrefixError`.
func (e *StripPrefixError) FmtDisplay(f *rs.Formatter) error {
	return f.Pad(e.String())
}

// NormalizeError is std::path::NormalizeError (unstable in Rust 1.99.1): the
// error NormalizeLexically returns if a parent reference (`..`) would point
// outside of the base directory.
type NormalizeError struct{}

var _ = rs.RegisterUnitStruct[NormalizeError]("NormalizeError")

// String holds the Display text.
func (e *NormalizeError) String() string {
	return "parent reference `..` points outside of base directory"
}

// Error makes NormalizeError a Go error with its Display text.
func (e *NormalizeError) Error() string { return e.String() }

// FmtDisplay is `impl Display for NormalizeError`.
func (e *NormalizeError) FmtDisplay(f *rs.Formatter) error {
	return f.WriteStr(e.String())
}

// ToStr is Path::to_str and OsStr::to_str: Some(path) if it is valid
// Unicode, None otherwise.
func (f Flavor) ToStr(path string) rs.Option[string] {
	return f.OsStrToStr(path)
}

// ToStringLossy is Path::to_string_lossy and OsStr::to_string_lossy: the
// path as text, with any non-Unicode sequences replaced by U+FFFD
// REPLACEMENT CHARACTER, as Rust replaces them.
func (f Flavor) ToStringLossy(path string) string {
	return f.OsStrToStringLossy(path)
}

// IsAbsolute is Path::is_absolute: true if the path is absolute, i.e., if it
// is independent of the current directory.
//
//   - On Unix, a path is absolute if it starts with the root, so
//     IsAbsolute and HasRoot are equivalent.
//
//   - On Windows, a path is absolute if it has a prefix and starts with the
//     root: `c:\windows` is absolute, while `c:temp` and `\temp` are not.
func (f Flavor) IsAbsolute(path string) bool {
	return f.sysIsAbsolute(path)
}

// IsRelative is Path::is_relative: true if the path is not absolute.
func (f Flavor) IsRelative(path string) bool {
	return !f.IsAbsolute(path)
}

// prefix is Path::prefix.
func (f Flavor) prefix(path string) rs.Option[Prefix] {
	return f.components(path).prefix
}

// HasRoot is Path::has_root: true if the path has a root.
//
//   - On Unix, a path has a root if it begins with `/`.
//
//   - On Windows, a path has a root if it:
//
//   - has no prefix and begins with a separator, e.g., `\windows`
//
//   - has a prefix followed by a separator, e.g., `c:\windows` but not
//     `c:windows`
//
//   - has any non-disk prefix, e.g., `\\server\share`
func (f Flavor) HasRoot(path string) bool {
	comps := f.components(path)
	return comps.hasRoot()
}

// Parent is Path::parent: the path without its final component, if there is
// one.
//
// This means it returns Some("") for relative paths with one component.
//
// Returns None if the path terminates in a root or prefix, or if it's the
// empty string.
func (f Flavor) Parent(path string) rs.Option[string] {
	comps := f.components(path)
	comp, ok := comps.NextBack().Get()
	if !ok {
		return rs.None[string]()
	}
	switch comp.(type) {
	case *Component_Normal, *Component_CurDir, *Component_ParentDir:
		return rs.Some(comps.AsPath())
	default:
		return rs.None[string]()
	}
}

// Ancestors is Path::ancestors: an iterator over the path and its ancestors.
//
// The iterator will yield the path that is returned if Parent is used zero
// or more times. If Parent returns None, the iterator will do likewise. The
// iterator will always yield at least one value, namely the path itself.
func (f Flavor) Ancestors(path string) *PathAncestors {
	return &PathAncestors{next: rs.Some(path), flavor: f.resolve()}
}

// FileName is Path::file_name: the final component of the path, if there is
// one.
//
// If the path is a normal file, this is the file name. If it's the path of
// a directory, this is the directory name.
//
// Returns None if the path terminates in `..`.
func (f Flavor) FileName(path string) rs.Option[string] {
	name, _, ok := f.fileNameEnd(path)
	return rs.OptionOf(name, ok)
}

// fileNameEnd is Path::file_name, with the offset in path just past the
// name.
func (f Flavor) fileNameEnd(path string) (name string, end int, ok bool) {
	comps := f.components(path)
	comp, end := comps.nextBackEnd()
	if comp, some := comp.Get(); some {
		if normal, isNormal := comp.(*Component_Normal); isNormal {
			return normal.F0, end, true
		}
	}
	return "", 0, false
}

// StripPrefix is Path::strip_prefix: a path that, when joined onto base,
// yields path.
//
// If base is not a prefix of path (i.e., StartsWith returns false), the
// error is a *StripPrefixError.
func (f Flavor) StripPrefix(path, base string) (string, error) {
	rest, ok := f.stripPrefix(path, base)
	if !ok {
		return "", &StripPrefixError{F0: rs.Unit{}}
	}
	return rest, nil
}

// TrimPrefix is Path::trim_prefix (unstable in Rust 1.99.1): the path
// without base in front, or the path itself when base is not a prefix of it.
func (f Flavor) TrimPrefix(path, base string) string {
	if rest, ok := f.stripPrefix(path, base); ok {
		return rest
	}
	return path
}

func (f Flavor) stripPrefix(path, base string) (string, bool) {
	rest, ok := iterAfter(f.components(path), f.components(base), false)
	if !ok {
		return "", false
	}
	return rest.AsPath(), true
}

// StartsWith is Path::starts_with: whether base is a prefix of path.
//
// Only considers whole path components to match.
func (f Flavor) StartsWith(path, base string) bool {
	_, ok := iterAfter(f.components(path), f.components(base), false)
	return ok
}

// EndsWith is Path::ends_with: whether child is a suffix of path.
//
// Only considers whole path components to match.
func (f Flavor) EndsWith(path, child string) bool {
	_, ok := iterAfter(f.components(path), f.components(child), true)
	return ok
}

// IsEmpty is Path::is_empty: whether the path is empty.
func (f Flavor) IsEmpty(path string) bool {
	return len(path) == 0
}

// FileStem is Path::file_stem: the stem (non-extension) portion of
// FileName.
//
// The stem is:
//
//   - None, if there is no file name;
//   - The entire file name if there is no embedded `.`;
//   - The entire file name if the file name begins with `.` and has no other `.`s within;
//   - Otherwise, the portion of the file name before the final `.`
func (f Flavor) FileStem(path string) rs.Option[string] {
	name, ok := f.FileName(path).Get()
	if !ok {
		return rs.None[string]()
	}
	before, after := rsplitFileAtDot(name)
	return before.Or(after)
}

// FilePrefix is Path::file_prefix: the prefix of the file name.
//
// The prefix is:
//
//   - None, if there is no file name;
//   - The entire file name if there is no embedded `.`;
//   - The portion of the file name before the first non-beginning `.`;
//   - The entire file name if the file name begins with `.` and has no other `.`s within;
//   - The portion of the file name before the second `.` if the file name begins with `.`
func (f Flavor) FilePrefix(path string) rs.Option[string] {
	name, ok := f.FileName(path).Get()
	if !ok {
		return rs.None[string]()
	}
	before, _ := splitFileAtDot(name)
	return rs.Some(before)
}

// Extension is Path::extension: the extension (without the leading dot) of
// FileName, if possible.
//
// The extension is:
//
//   - None, if there is no file name;
//   - None, if there is no embedded `.`;
//   - None, if the file name begins with `.` and has no other `.`s within;
//   - Otherwise, the portion of the file name after the final `.`
func (f Flavor) Extension(path string) rs.Option[string] {
	name, ok := f.FileName(path).Get()
	if !ok {
		return rs.None[string]()
	}
	before, after := rsplitFileAtDot(name)
	if before.IsSome() {
		return after
	}
	return rs.None[string]()
}

// HasTrailingSep is Path::has_trailing_sep (unstable in Rust 1.99.1):
// whether the path ends in a trailing separator.
//
// This is generally done to ensure that a path is treated as a directory,
// not a file, although it does not actually guarantee that such a path is a
// directory on the underlying file system.
func (f Flavor) HasTrailingSep(path string) bool {
	return len(path) != 0 && f.isSepByte(path[len(path)-1])
}

// WithTrailingSep is Path::with_trailing_sep (unstable in Rust 1.99.1): it
// ensures that a path has a trailing separator, allocating a PathBuf if
// necessary.
//
// The resulting path will return true for HasTrailingSep.
func (f Flavor) WithTrailingSep(path string) string {
	if f.HasTrailingSep(path) {
		return path
	}
	return f.Join(path, "")
}

// TrimTrailingSep is Path::trim_trailing_sep (unstable in Rust 1.99.1): it
// trims a trailing separator from a path, if possible.
//
// The resulting path will return false for HasTrailingSep for most paths.
//
// Some paths, like `/`, cannot be trimmed in this way.
func (f Flavor) TrimTrailingSep(path string) string {
	if f.HasTrailingSep(path) && (!f.HasRoot(path) || f.Parent(path).IsSome()) {
		bytes := path
		for len(bytes) != 0 && f.isSepByte(bytes[len(bytes)-1]) {
			bytes = bytes[:len(bytes)-1]
		}
		return bytes
	}
	return path
}

// Join is Path::join: an owned PathBuf with path adjoined to base.
//
// If path is absolute, it replaces the current path.
//
// See Push for more details on what it means to adjoin a path. Nothing is
// cleaned: Join("a", "../b") is "a/../b".
func (f Flavor) Join(base, path string) string {
	buf := base
	f.Push(&buf, path)
	return buf
}

// WithFileName is Path::with_file_name: an owned PathBuf like path but with
// the given file name.
//
// See SetFileName for more details.
func (f Flavor) WithFileName(path, fileName string) string {
	buf := path
	f.SetFileName(&buf, fileName)
	return buf
}

// WithExtension is Path::with_extension: an owned PathBuf like path but
// with the given extension.
//
// See SetExtension for more details.
func (f Flavor) WithExtension(path, extension string) string {
	selfLen := len(path)
	selfBytes := path

	var sliceToCopy string
	if previousExtension, ok := f.Extension(path).Get(); !ok {
		// Enough capacity for the extension and the dot
		wholePath := selfBytes
		sliceToCopy = wholePath
	} else {
		pathTillDot := selfBytes[:selfLen-len(previousExtension)]
		sliceToCopy = pathTillDot
	}

	newPath := sliceToCopy
	f.SetExtension(&newPath, extension)
	return newPath
}

// WithAddedExtension is Path::with_added_extension: an owned PathBuf like
// path but with the extension added.
//
// See AddExtension for more details.
func (f Flavor) WithAddedExtension(path, extension string) string {
	newPath := path
	f.AddExtension(&newPath, extension)
	return newPath
}

// Components is Path::components: an iterator over the Components of the
// path.
//
// When parsing the path, there is a small amount of normalization:
//
//   - Repeated separators are ignored, so `a/b` and `a//b` both have
//     `a` and `b` as components.
//
//   - Occurrences of `.` are normalized away, except if they are at the
//     beginning of the path. For example, `a/./b`, `a/b/`, `a/b/.` and
//     `a/b` all have `a` and `b` as components, but `./a/b` starts with
//     an additional CurDir component.
//
//   - Trailing separators are normalized away, so `/a/b` and `/a/b/` are
//     equivalent.
//
// Note that no other normalization takes place; in particular, `a/c`
// and `a/b/../c` are distinct, to account for the possibility that `b`
// is a symbolic link (so its parent isn't `a`).
func (f Flavor) Components(path string) *PathComponents {
	comps := f.components(path)
	return &comps
}

func (f Flavor) components(path string) PathComponents {
	f = f.resolve()
	prefix := f.parsePrefix(path)
	// use a platform-specific initial state to avoid one turn of
	// the state-machine when the platform doesn't have a Prefix.
	front := stateStartDir
	if f.hasPrefixes() {
		front = statePrefix
	}
	return PathComponents{
		path:            path,
		prefix:          prefix,
		hasPhysicalRoot: f.hasPhysicalRoot(path, prefix),
		front:           front,
		back:            stateBody,
		flavor:          f,
	}
}

// Iter is Path::iter: an iterator over the path viewed as OsStr slices.
//
// For more information about the particulars of how the path is separated
// into components, see Components.
func (f Flavor) Iter(path string) *PathIter {
	return &PathIter{inner: f.components(path)}
}

// Display is Path::display and OsStr::display made into text: the path for
// safely printing, which may be a lossy conversion. It is the same text as
// ToStringLossy.
//
// A Rust format string may give a `{}` filled by `path.display()` a width
// or a precision. Padding and truncating this text as a str gives what Rust
// prints, except for a Windows path that holds an unpaired surrogate, which
// Rust writes without applying the flags.
func (f Flavor) Display(path string) string {
	return f.OsStrToStringLossy(path)
}

// NormalizeLexically is Path::normalize_lexically (unstable in Rust
// 1.99.1): it normalizes a path, including `..` without traversing the
// filesystem.
//
// It fails, with a *NormalizeError, if normalizing the path would cause it
// to leave its root or the directory it started in.
func (f Flavor) NormalizeLexically(path string) (string, error) {
	f = f.resolve()
	lexical := ""
	iter := f.components(path)

	// Find the root, if any, and add it to the lexical path.
	// Here we treat the Windows path "C:\" as a single "root" even though
	// `components` splits it into two: (Prefix, RootDir).
	var root int
	peek := iter
	first, ok := peek.Next().Get()
	if !ok {
		return "", nil
	}
	switch first := first.(type) {
	case *Component_ParentDir:
		return "", &NormalizeError{}
	case *Component_RootDir, *Component_CurDir:
		f.Push(&lexical, f.ComponentAsOsStr(first))
		iter.Next()
		root = len(lexical)
	case *Component_Prefix:
		f.Push(&lexical, first.F0.AsOsStr())
		iter.Next()
		peek = iter
		if second, ok := peek.Next().Get(); ok && rs.Is[*Component_RootDir](second) {
			f.Push(&lexical, f.ComponentAsOsStr(second))
			iter.Next()
		}
		root = len(lexical)
	case *Component_Normal:
		root = 0
	default:
		panic(rs.Unreachable(first))
	}

	for component := range iter.All() {
		switch component := component.(type) {
		case *Component_RootDir:
			panic(rs.Unreachable())
		case *Component_Prefix:
			return "", &NormalizeError{}
		case *Component_CurDir:
			continue
		case *Component_ParentDir:
			// It's an error if ParentDir causes us to go above the "root".
			if len(lexical) == root {
				return "", &NormalizeError{}
			}
			f.Pop(&lexical)
		case *Component_Normal:
			f.Push(&lexical, component.F0)
		default:
			panic(rs.Unreachable(component))
		}
	}
	return lexical, nil
}

// PathDebug is a path or an OsStr as the argument of `{:?}`. Rust formats
// the path itself; a Go string would be formatted as a str, which differs
// where the path is not Unicode:
//
//	format!("no such file: {:?}", path)
//	rs.Format("no such file: {:?}", rspath.Debug(path))
type PathDebug struct {
	path   string
	flavor Flavor
}

// String holds the Debug text: the path in double quotes, escaped as Rust
// escapes it.
func (d PathDebug) String() string {
	return d.flavor.DebugString(d.path)
}

// FmtDebug is `impl Debug for Path` and `impl Debug for OsStr`.
func (d PathDebug) FmtDebug(f *rs.Formatter) error {
	return f.WriteStr(d.String())
}

// Debug wraps a path or an OsStr for `{:?}`.
func (f Flavor) Debug(path string) PathDebug {
	return PathDebug{path: path, flavor: f.resolve()}
}

// DebugString is `format!("{:?}", path)` for a Path, PathBuf, OsStr or
// OsString: the text in double quotes, escaped as Rust escapes a str, with
// what is not Unicode written as \xNN bytes on Unix and as \u{d800}
// surrogates on Windows.
func (f Flavor) DebugString(path string) string {
	if f.resolve() == Windows {
		return wtf8Debug(path)
	}
	return utf8ChunksDebug(path)
}

// Eq is `a == b` for two paths. Rust compares paths by their components, so
// `a/b`, `a//b`, `a/./b` and `a/b/` are all equal, and on Windows `c:\a` and
// `C:/a` are too. Go's == on the strings does not say that.
func (f Flavor) Eq(a, b string) bool {
	left, right := f.components(a), f.components(b)
	return left.Equal(&right)
}

// Cmp is `a.cmp(&b)` for two paths: negative, zero or positive. Rust orders
// paths component by component, which is not the order of the strings:
// `a/b` sorts before `a.b`, and a BTreeMap keyed by PathBuf iterates in this
// order.
func (f Flavor) Cmp(a, b string) int {
	return compareComponents(f.components(a), f.components(b))
}

// HashKey is a key that is the same for two paths exactly when Eq says they
// are equal. It is what a Go map or set keyed by a path must be keyed by to
// hold the entries a Rust HashMap or HashSet keyed by PathBuf holds.
//
// The key is not a path and is not what Rust feeds its hasher: it is the
// component list, written so that different lists cannot collide.
func (f Flavor) HashKey(path string) string {
	var b strings.Builder
	writeField := func(field string) {
		writeUvarint(&b, uint64(len(field)))
		b.WriteString(field)
	}
	for component := range f.Components(path).All() {
		switch component := component.(type) {
		case *Component_Prefix:
			b.WriteByte('P')
			switch prefix := component.F0.parsed.(type) {
			case *Prefix_Verbatim:
				b.WriteByte('0')
				writeField(prefix.F0)
			case *Prefix_VerbatimUNC:
				b.WriteByte('1')
				writeField(prefix.F0)
				writeField(prefix.F1)
			case *Prefix_VerbatimDisk:
				b.WriteByte('2')
				b.WriteByte(prefix.F0)
			case *Prefix_DeviceNS:
				b.WriteByte('3')
				writeField(prefix.F0)
			case *Prefix_UNC:
				b.WriteByte('4')
				writeField(prefix.F0)
				writeField(prefix.F1)
			case *Prefix_Disk:
				b.WriteByte('5')
				b.WriteByte(prefix.F0)
			default:
				panic(rs.Unreachable(prefix))
			}
		case *Component_RootDir:
			b.WriteByte('R')
		case *Component_CurDir:
			b.WriteByte('C')
		case *Component_ParentDir:
			b.WriteByte('U')
		case *Component_Normal:
			b.WriteByte('N')
			writeField(component.F0)
		default:
			panic(rs.Unreachable(component))
		}
	}
	return b.String()
}

func writeUvarint(b *strings.Builder, value uint64) {
	for value >= 0x80 {
		b.WriteByte(byte(value) | 0x80)
		value >>= 7
	}
	b.WriteByte(byte(value))
}

// componentDebugString is `format!("{:?}", component)`: the derived Debug of
// Component.
func (f Flavor) componentDebugString(c Component) string {
	switch c := c.(type) {
	case *Component_Prefix:
		return "Prefix(PrefixComponent { raw: " + f.DebugString(c.F0.raw) +
			", parsed: " + f.prefixDebugString(c.F0.parsed) + " })"
	case *Component_RootDir:
		return "RootDir"
	case *Component_CurDir:
		return "CurDir"
	case *Component_ParentDir:
		return "ParentDir"
	case *Component_Normal:
		return "Normal(" + f.DebugString(c.F0) + ")"
	default:
		panic(rs.Unreachable(c))
	}
}

// prefixDebugString is `format!("{:?}", prefix)`: the derived Debug of
// Prefix.
func (f Flavor) prefixDebugString(p Prefix) string {
	switch p := p.(type) {
	case *Prefix_Verbatim:
		return "Verbatim(" + f.DebugString(p.F0) + ")"
	case *Prefix_VerbatimUNC:
		return "VerbatimUNC(" + f.DebugString(p.F0) + ", " + f.DebugString(p.F1) + ")"
	case *Prefix_VerbatimDisk:
		return "VerbatimDisk(" + utoa(p.F0) + ")"
	case *Prefix_DeviceNS:
		return "DeviceNS(" + f.DebugString(p.F0) + ")"
	case *Prefix_UNC:
		return "UNC(" + f.DebugString(p.F0) + ", " + f.DebugString(p.F1) + ")"
	case *Prefix_Disk:
		return "Disk(" + utoa(p.F0) + ")"
	default:
		panic(rs.Unreachable(p))
	}
}

func utoa(value uint8) string {
	digits := [3]byte{}
	i := len(digits)
	for {
		i--
		digits[i] = '0' + value%10
		value /= 10
		if value == 0 {
			return string(digits[i:])
		}
	}
}

// The derived Debug of Component, PrefixComponent and Prefix prints each
// OsStr they hold with the OsStr's own Debug. The formatter would print the
// Go string that holds one as a str, which differs where the OsStr is not
// Unicode: a Windows OsStr writes an unpaired surrogate as `\u{d800}`. So the
// variants that hold an OsStr write themselves, as derive(Debug) would, with
// the OsStr written by the host's flavour; the variants without one are
// printed from their registration.

// FmtDebug is the derived Debug of Component::Normal.
func (c *Component_Normal) FmtDebug(f *rs.Formatter) error {
	return componentDebug{flavor: Host, component: c}.FmtDebug(f)
}

// FmtDebug is the derived Debug of PrefixComponent.
func (p PrefixComponent) FmtDebug(f *rs.Formatter) error {
	return prefixComponentDebug{flavor: Host, prefix: p}.FmtDebug(f)
}

// FmtDebug is the derived Debug of Prefix::Verbatim.
func (p *Prefix_Verbatim) FmtDebug(f *rs.Formatter) error {
	return prefixDebug{flavor: Host, prefix: p}.FmtDebug(f)
}

// FmtDebug is the derived Debug of Prefix::VerbatimUNC.
func (p *Prefix_VerbatimUNC) FmtDebug(f *rs.Formatter) error {
	return prefixDebug{flavor: Host, prefix: p}.FmtDebug(f)
}

// FmtDebug is the derived Debug of Prefix::DeviceNS.
func (p *Prefix_DeviceNS) FmtDebug(f *rs.Formatter) error {
	return prefixDebug{flavor: Host, prefix: p}.FmtDebug(f)
}

// FmtDebug is the derived Debug of Prefix::UNC.
func (p *Prefix_UNC) FmtDebug(f *rs.Formatter) error {
	return prefixDebug{flavor: Host, prefix: p}.FmtDebug(f)
}

// componentDebug is a Component as the argument of `{:?}` under a flavour.
type componentDebug struct {
	flavor    Flavor
	component Component
}

func (d componentDebug) FmtDebug(f *rs.Formatter) error {
	switch c := d.component.(type) {
	case *Component_Prefix:
		return f.DebugTuple("Prefix").Field(prefixComponentDebug{flavor: d.flavor, prefix: c.F0}).Finish()
	case *Component_RootDir:
		return f.WriteStr("RootDir")
	case *Component_CurDir:
		return f.WriteStr("CurDir")
	case *Component_ParentDir:
		return f.WriteStr("ParentDir")
	case *Component_Normal:
		return f.DebugTuple("Normal").Field(d.flavor.Debug(c.F0)).Finish()
	default:
		panic(rs.Unreachable(c))
	}
}

// prefixComponentDebug is a PrefixComponent as the argument of `{:?}` under
// a flavour.
type prefixComponentDebug struct {
	flavor Flavor
	prefix PrefixComponent
}

func (d prefixComponentDebug) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("PrefixComponent").
		Field("raw", d.flavor.Debug(d.prefix.raw)).
		Field("parsed", prefixDebug{flavor: d.flavor, prefix: d.prefix.parsed}).
		Finish()
}

// prefixDebug is a Prefix as the argument of `{:?}` under a flavour.
type prefixDebug struct {
	flavor Flavor
	prefix Prefix
}

func (d prefixDebug) FmtDebug(f *rs.Formatter) error {
	switch p := d.prefix.(type) {
	case *Prefix_Verbatim:
		return f.DebugTuple("Verbatim").Field(d.flavor.Debug(p.F0)).Finish()
	case *Prefix_VerbatimUNC:
		return f.DebugTuple("VerbatimUNC").Field(d.flavor.Debug(p.F0)).Field(d.flavor.Debug(p.F1)).Finish()
	case *Prefix_VerbatimDisk:
		return f.DebugTuple("VerbatimDisk").Field(p.F0).Finish()
	case *Prefix_DeviceNS:
		return f.DebugTuple("DeviceNS").Field(d.flavor.Debug(p.F0)).Finish()
	case *Prefix_UNC:
		return f.DebugTuple("UNC").Field(d.flavor.Debug(p.F0)).Field(d.flavor.Debug(p.F1)).Finish()
	case *Prefix_Disk:
		return f.DebugTuple("Disk").Field(p.F0).Finish()
	default:
		panic(rs.Unreachable(p))
	}
}
