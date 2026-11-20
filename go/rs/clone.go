// Port of core 1.99.1 src/clone.rs (what #[derive(Clone)] generates, done by
// reflection), and the description of Go types that eq.go, cmp.go and
// default.go share with it.

package rs

import (
	"reflect"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"unsafe"
)

// How a value is cloned.
//
// The Go types of the port stand for Rust types (translation-rules.md,
// rule 3), and Clone copies a value the way the derived Clone of the Rust type
// would:
//
//   - A number, bool, string, data-free enum, func or channel is copied as it
//     is.
//   - A slice, an array, a Go map, an Option, a Result, a tuple and a struct
//     are copied element by element and field by field, unexported fields
//     included. A nil slice or map stays nil.
//   - A collection that implements ReflectMap or ReflectSeq is rebuilt, entry
//     by entry in iteration order, in a new zero value of its type, even
//     when it has a Clone method of its own (findMethods says why).
//   - A pointer is a Box<T>: the copy points to a new allocation holding a
//     clone of the pointee.
//   - A struct field tagged `rs:"name,arc"` holds an Arc<T> or Rc<T>, and what
//     the Arc points to is shared, not copied. Under the tag the field is
//     still copied container by container (slices, arrays, maps, options,
//     tuples and collections are duplicated), but every pointer and interface
//     value met on the way is copied as it is, and so is a slice that has
//     none of those below it ([]byte, []Item), which is the Arc<[T]> itself.
//     So `[]*T` is a Vec<Arc<T>> (new slice, shared pointers), `Option[*T]` an
//     Option<Arc<T>>, `[]T` an Arc<[T]> (shared slice). A struct that is not
//     one of those containers ends the tag's reach: it is cloned by its own
//     fields and tags. A field whose type mixes shared and owned pointers in
//     a way this cannot tell apart needs a hand-written Clone.
//   - An interface value is a Rust enum when it holds a pointer to a
//     registered variant, or when the interface has an unexported method (the
//     sealed interfaces of rule 3): the variant is owned and is copied
//     deeply. Any other pointer held in an interface is a trait object
//     (Arc<dyn Trait>) and is shared. A value that is not a pointer (a
//     string or a slice in an `any`) is cloned as its own type.
//   - A type with a method `Clone() T`, on the value or on the pointer, is
//     copied by that method wherever it appears. A pointer type with a method
//     `Clone() *T` is a handle and is cloned by it.
//
// Some types must never be copied field by field, because a copy would
// duplicate state that has one owner: locks, atomics, channel ends, task
// handles. Clone copies a value of such a type by plain assignment and shares
// a pointer to one. They are:
//
//   - every type of the Go packages sync and sync/atomic;
//   - a struct that embeds NoReflectClone, the marker runtime packages use;
//   - a struct that holds one of those by value.
//
// A struct of any other Go standard library package is a value the library
// designed for assignment (time.Time): it is copied by assignment, and a
// pointer to one is copied into a new allocation when the type is comparable
// and shared when it is not (*bytes.Buffer, *regexp.Regexp).
//
// Rust code cannot build a cycle out of owned values, so nothing here looks
// for one.

// NoReflectClone marks a type that Clone must not copy field by field. Embed
// it, first so that it takes no room:
//
//	type SyncMpscSender[T any] struct {
//		rs.NoReflectClone
//		channel *channel[T]
//	}
//
// A value of the type is then copied by plain assignment and a pointer to it
// is shared, unless the type has a Clone method, which is used instead. Eq
// compares two pointers to such a type by identity.
type NoReflectClone struct{}

func (NoReflectClone) rsNoReflectClone() {}

// NoReflectCloner is the marker interface: what embedding NoReflectClone
// implements, and the only way to implement it outside this package. A
// runtime package can hold its handle types to it:
//
//	var _ rs.NoReflectCloner = (*SyncMpscSender[int])(nil)
type NoReflectCloner interface{ rsNoReflectClone() }

// shapeKind says which rule applies to a Go type.
type shapeKind uint8

const (
	kindBool shapeKind = iota + 1
	kindInt
	kindUint
	kindFloat
	kindComplex
	kindString
	// kindHandle is a channel, a func or an unsafe.Pointer: one pointer word,
	// copied as it is and compared by identity.
	kindHandle
	kindPointer
	kindInterface
	kindSlice
	kindArray
	kindGoMap
	kindStruct
	// kindOption and kindResult are rs.Option and rs.Result.
	kindOption
	kindResult
	// kindReflectMap and kindReflectSeq are collections that describe
	// themselves through ReflectMap and ReflectSeq.
	kindReflectMap
	kindReflectSeq
	// kindArc is rs.Arc: cloned by its Clone method, which makes another
	// handle, and compared by the value it holds, as a pointer is.
	kindArc
)

// copyKind says how a plain copy of a type is made.
type copyKind uint8

const (
	// copyTyped copies through reflection, which keeps the garbage
	// collector's write barriers for memory that holds pointers.
	copyTyped copyKind = iota
	copy1
	copy2
	copy4
	copy8
	copy16
	copyComplex64
	copyString
	copyPointer
	copyNothing
)

// shape is what the structural operations know about one Go type. Shapes are
// built once and are immutable after shapeOf returns them.
type shape struct {
	typ  reflect.Type
	kind shapeKind
	size uintptr
	copy copyKind

	// elem is the pointee, the element of a slice or array, the value of a
	// map, or the payload of an Option.
	elem *shape
	// key is the key of a Go map or of a ReflectMap.
	key *shape
	// fields are a struct's fields in declaration order, without blank ones.
	fields []shapeField

	// The hand-written Clone, PartialEq and Ord of the type.
	clone   method
	equal   method
	compare method

	// container is a struct an `arc` tag reaches through: Option, Result,
	// the tuples, Cell, RefCell, Reverse.
	container bool
	// reversed is rs.Reverse, whose order is that of its field turned round.
	reversed bool
	// bitCopy is rs.Cell, whose Clone copies the value as it is and never
	// calls the value's own Clone.
	bitCopy bool
	// sealed is an interface with an unexported method: a Rust enum.
	sealed bool
	// pinned is a type that must not be duplicated (a lock, an atomic, a
	// NoReflectClone type, or a struct holding one). A pointer to it is
	// shared.
	pinned bool
	// stdlib is a struct of the Go standard library, whose fields belong to
	// the library.
	stdlib bool
	// opaque is a struct copied by assignment: pinned, or from the Go
	// standard library.
	opaque bool
	// sharedBehindPointer says that cloning a pointer to this type copies
	// the pointer.
	sharedBehindPointer bool
	// plain says that an owned clone of the type is a plain copy, and
	// arcPlain that a clone under an `arc` tag is.
	plain    bool
	arcPlain bool
	// arcLeafy says that the type holds, through containers, something an
	// `arc` tag shares, or another container that must be copied; a slice of
	// such elements is copied, any other slice is the Arc<[T]> itself.
	arcLeafy bool

	// reflexive caches whether x == x holds for every value of the type:
	// 0 unknown, 1 yes, 2 no.
	reflexive atomic.Int32
}

type shapeField struct {
	name   string
	offset uintptr
	shape  *shape
	// arc is the tag option: the field holds an Arc<T> or Rc<T>.
	arc bool
	// tagged says that the field carries an `rs` tag, which marks its struct
	// as the port of a Rust struct.
	tagged bool
}

// method is a hand-written trait method found on a type.
type method struct {
	ok bool
	// index is the method's position in the method set of the pointer type,
	// which holds the methods of both receivers.
	index int
	// form is the type of the argument (Equal, Compare) or of the result
	// (Clone): the type itself, a pointer to it, or an interface.
	form methodForm
	// param is the argument's type when it is an interface: an enum's
	// hand-written PartialEq or Ord takes the enum, so the other operand may
	// be another variant.
	param reflect.Type
}

type methodForm uint8

const (
	formValue methodForm = iota
	formPointer
	formInterface
)

var (
	rsPkgPath            = reflect.TypeFor[Unit]().PkgPath()
	noReflectCloneType   = reflect.TypeFor[NoReflectClone]()
	noReflectClonerType  = reflect.TypeFor[NoReflectCloner]()
	reflectMapType       = reflect.TypeFor[ReflectMap]()
	reflectSeqType       = reflect.TypeFor[ReflectSeq]()
	arcValueType         = reflect.TypeFor[arcValue]()
	rsContainerTypeNames = []string{"Option", "Result", "Tuple2", "Tuple3", "Tuple4", "Tuple5", "Cell", "RefCell", "Reverse"}
)

var (
	// shapes maps a reflect.Type to its finished *shape.
	shapes sync.Map
	// shapeBuildMu serializes building, so that a group of types that refer
	// to each other is published complete.
	shapeBuildMu sync.Mutex
)

// shapeOf returns the shape of t, building it and the shapes it refers to on
// first use.
func shapeOf(t reflect.Type) *shape {
	if s, ok := shapes.Load(t); ok {
		return s.(*shape)
	}
	shapeBuildMu.Lock()
	defer shapeBuildMu.Unlock()
	builder := shapeBuilder{built: map[reflect.Type]*shape{}}
	s := builder.build(t)
	builder.finish()
	return s
}

type shapeBuilder struct {
	// built holds the shapes of this build, which may refer to each other
	// before they are complete.
	built map[reflect.Type]*shape
	order []*shape
}

func (b *shapeBuilder) build(t reflect.Type) *shape {
	if s, ok := shapes.Load(t); ok {
		return s.(*shape)
	}
	if s, ok := b.built[t]; ok {
		return s
	}
	s := &shape{typ: t, size: t.Size()}
	b.built[t] = s
	b.order = append(b.order, s)

	switch t.Kind() {
	case reflect.Bool:
		s.kind = kindBool
	case reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64:
		s.kind = kindInt
	case reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64, reflect.Uintptr:
		s.kind = kindUint
	case reflect.Float32, reflect.Float64:
		s.kind = kindFloat
	case reflect.Complex64, reflect.Complex128:
		s.kind = kindComplex
	case reflect.String:
		s.kind = kindString
	case reflect.Chan, reflect.Func, reflect.UnsafePointer:
		s.kind = kindHandle
	case reflect.Pointer:
		s.kind = kindPointer
		s.elem = b.build(t.Elem())
	case reflect.Interface:
		s.kind = kindInterface
		for i := range t.NumMethod() {
			if !t.Method(i).IsExported() {
				s.sealed = true
			}
		}
	case reflect.Slice:
		s.kind = kindSlice
		s.elem = b.build(t.Elem())
	case reflect.Array:
		s.kind = kindArray
		s.elem = b.build(t.Elem())
	case reflect.Map:
		s.kind = kindGoMap
		s.key = b.build(t.Key())
		s.elem = b.build(t.Elem())
	case reflect.Struct:
		b.buildStruct(s)
	default:
		panic("rs: no structural clone, equality or ordering for " + t.String())
	}
	s.findMethods()
	return s
}

func (b *shapeBuilder) buildStruct(s *shape) {
	t := s.typ
	pointer := reflect.PointerTo(t)
	switch {
	case isRsGeneric(t, "Option"):
		s.kind = kindOption
	case isRsGeneric(t, "Result"):
		s.kind = kindResult
	case pointer.Implements(reflectMapType):
		s.kind = kindReflectMap
		collection := reflect.New(t).Interface().(ReflectMap)
		s.key = b.build(collection.ReflectKeyType())
		s.elem = b.build(collection.ReflectValueType())
		return
	case pointer.Implements(reflectSeqType):
		s.kind = kindReflectSeq
		s.elem = b.build(reflect.New(t).Interface().(ReflectSeq).ReflectElemType())
		return
	case pointer.Implements(arcValueType):
		s.kind = kindArc
		s.elem = b.build(reflect.New(t).Interface().(arcValue).ReflectArcType())
		return
	default:
		s.kind = kindStruct
	}
	for _, name := range rsContainerTypeNames {
		if isRsGeneric(t, name) {
			s.container = true
		}
	}
	s.reversed = isRsGeneric(t, "Reverse")
	s.bitCopy = isRsGeneric(t, "Cell")
	for i := range t.NumField() {
		field := t.Field(i)
		if field.Name == "_" {
			continue
		}
		_, options := FieldRustName(field)
		_, tagged := field.Tag.Lookup("rs")
		s.fields = append(s.fields, shapeField{
			name:   field.Name,
			offset: field.Offset,
			shape:  b.build(field.Type),
			arc:    slices.Contains(options, "arc"),
			tagged: tagged,
		})
	}
	if s.kind == kindOption {
		if len(s.fields) != 2 || s.fields[0].name != "value" || s.fields[1].name != "some" {
			panic("rs: Option no longer has the fields value and some")
		}
		s.elem = s.fields[0].shape
	}
	if s.kind == kindResult {
		if len(s.fields) != 2 || s.fields[0].name != "Value" || s.fields[1].name != "Err" {
			panic("rs: Result no longer has the fields Value and Err")
		}
		s.elem = s.fields[0].shape
	}
}

// isRsGeneric reports whether t is an instantiation of the generic type name
// of this package.
func isRsGeneric(t reflect.Type, name string) bool {
	return t.PkgPath() == rsPkgPath && strings.HasPrefix(t.Name(), name+"[")
}

// isStdlibPackage reports whether a package path belongs to the Go standard
// library: its first element has no dot. A program's own package is "main".
func isStdlibPackage(path string) bool {
	if path == "" || path == "main" || path == "command-line-arguments" {
		return false
	}
	first, _, _ := strings.Cut(path, "/")
	return !strings.Contains(first, ".")
}

// findMethods looks for the hand-written Clone, Equal and Compare of a type.
// Their signatures are those of translation-rules.md, rule 2; a method of the
// same name with another signature (promoted from an embedded field, for
// example) is not one of them.
//
// A collection that describes itself through ReflectMap or ReflectSeq is
// cloned, compared and ordered through those, element by element, even when
// it has Clone, Equal and Compare methods for direct calls: only so does an
// `arc` tag on the field holding it reach its elements, and only so does a
// NaN among them leave it unordered, as a Compare method's int cannot say.
func (s *shape) findMethods() {
	t := s.typ
	if t.Name() == "" || t.Kind() == reflect.Interface || t.Kind() == reflect.Pointer ||
		s.kind == kindReflectMap || s.kind == kindReflectSeq {
		return
	}
	pointer := reflect.PointerTo(t)
	form := func(candidate reflect.Type) (methodForm, bool) {
		switch {
		case candidate == t:
			return formValue, true
		case candidate == pointer:
			return formPointer, true
		case candidate.Kind() == reflect.Interface && pointer.Implements(candidate):
			return formInterface, true
		}
		return 0, false
	}
	if m, found := pointer.MethodByName("Clone"); found && m.Type.NumIn() == 1 && m.Type.NumOut() == 1 {
		if f, ok := form(m.Type.Out(0)); ok {
			s.clone = method{ok: true, index: m.Index, form: f}
		}
	}
	if m, found := pointer.MethodByName("Equal"); found && m.Type.NumIn() == 2 && m.Type.NumOut() == 1 &&
		m.Type.Out(0).Kind() == reflect.Bool {
		if f, ok := form(m.Type.In(1)); ok {
			s.equal = method{ok: true, index: m.Index, form: f, param: m.Type.In(1)}
		}
	}
	if m, found := pointer.MethodByName("Compare"); found && m.Type.NumIn() == 2 && m.Type.NumOut() == 1 {
		switch m.Type.Out(0).Kind() {
		case reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64:
			if f, ok := form(m.Type.In(1)); ok {
				s.compare = method{ok: true, index: m.Index, form: f, param: m.Type.In(1)}
			}
		}
	}
}

// finish computes the flags of the shapes of this build and publishes them.
// A flag of a type depends only on the types it holds by value, and on the
// by-value flags of a pointee, so the recursion below ends even for types
// that refer to themselves through pointers and slices.
func (b *shapeBuilder) finish() {
	valueDone := map[*shape]bool{}
	cloneDone := map[*shape]bool{}
	for _, s := range b.order {
		b.resolveValueFlags(s, valueDone)
	}
	for _, s := range b.order {
		b.resolveCloneFlags(s, valueDone, cloneDone)
	}
	for _, s := range b.order {
		shapes.Store(s.typ, s)
	}
}

func (b *shapeBuilder) isNew(s *shape) bool {
	built, ok := b.built[s.typ]
	return ok && built == s
}

// resolveValueFlags sets pinned, opaque and sharedBehindPointer.
func (b *shapeBuilder) resolveValueFlags(s *shape, done map[*shape]bool) {
	if !b.isNew(s) || done[s] {
		return
	}
	done[s] = true
	t := s.typ
	switch s.kind {
	case kindArray:
		b.resolveValueFlags(s.elem, done)
		s.pinned = s.elem.pinned
	case kindStruct, kindOption, kindResult, kindReflectMap, kindReflectSeq, kindArc:
		stdlib := isStdlibPackage(t.PkgPath())
		switch {
		case t.PkgPath() == "sync" || t.PkgPath() == "sync/atomic":
			s.pinned = true
		case s.clone.ok:
			// The type says how it is cloned.
		case t == noReflectCloneType || reflect.PointerTo(t).Implements(noReflectClonerType):
			s.pinned = true
		default:
			for _, field := range s.fields {
				b.resolveValueFlags(field.shape, done)
				if field.shape.pinned {
					s.pinned = true
				}
			}
		}
		s.stdlib = stdlib
		s.opaque = s.pinned || stdlib
		s.sharedBehindPointer = s.pinned || (stdlib && !t.Comparable())
	}
}

// resolveCloneFlags sets copy, plain, arcPlain and arcLeafy.
func (b *shapeBuilder) resolveCloneFlags(s *shape, valueDone, done map[*shape]bool) {
	if !b.isNew(s) || done[s] {
		return
	}
	done[s] = true
	s.copy = copyTyped
	if s.size == 0 {
		s.copy = copyNothing
	}
	scalar := func() {
		switch s.size {
		case 1:
			s.copy = copy1
		case 2:
			s.copy = copy2
		case 4:
			s.copy = copy4
		case 8:
			s.copy = copy8
		case 16:
			s.copy = copy16
		}
		s.plain, s.arcPlain = true, true
	}
	switch s.kind {
	case kindBool, kindInt, kindUint, kindFloat:
		scalar()
	case kindComplex:
		scalar()
		if s.size == 8 {
			// Two float32: aligned to four bytes, not eight.
			s.copy = copyComplex64
		}
	case kindString:
		s.copy = copyString
		s.plain, s.arcPlain, s.arcLeafy = true, true, true
	case kindHandle:
		s.copy = copyPointer
		s.plain, s.arcPlain, s.arcLeafy = true, true, true
	case kindPointer:
		s.copy = copyPointer
		b.resolveValueFlags(s.elem, valueDone)
		s.plain = !s.elem.clone.ok && s.elem.sharedBehindPointer
		s.arcPlain, s.arcLeafy = true, true
	case kindInterface:
		s.arcPlain, s.arcLeafy = true, true
	case kindSlice:
		b.resolveCloneFlags(s.elem, valueDone, done)
		s.arcPlain = !s.elem.arcLeafy
		s.arcLeafy = true
	case kindArray:
		b.resolveCloneFlags(s.elem, valueDone, done)
		s.plain = s.elem.plain || s.typ.Len() == 0
		s.arcPlain = s.elem.arcPlain || s.typ.Len() == 0
		s.arcLeafy = s.elem.arcLeafy
	case kindGoMap, kindReflectMap, kindReflectSeq:
		s.arcLeafy = true
	case kindStruct, kindOption, kindResult:
		if s.opaque || s.bitCopy {
			s.plain, s.arcPlain = true, true
			break
		}
		plain, arcPlain := true, true
		for _, field := range s.fields {
			b.resolveCloneFlags(field.shape, valueDone, done)
			if field.arc {
				plain = plain && field.shape.arcPlain
			} else {
				plain = plain && field.shape.plain
			}
			arcPlain = arcPlain && field.shape.arcPlain
			if s.container && field.shape.arcLeafy {
				s.arcLeafy = true
			}
		}
		s.plain = plain
		if s.container {
			s.arcPlain = arcPlain
		} else {
			s.arcPlain = plain
		}
	}
	if s.clone.ok {
		// A hand-written Clone runs wherever the value is, tag or no tag.
		s.plain, s.arcPlain, s.arcLeafy = false, false, true
	}
}

// cloneValue is the body of Clone.
func cloneValue[T any](value T) T {
	s := shapeOf(reflect.TypeFor[T]())
	if s.clone.ok {
		if cloner, ok := any(&value).(interface{ Clone() T }); ok {
			return cloner.Clone()
		}
	}
	if s.plain {
		return value
	}
	var out T
	s.cloneAt(unsafe.Pointer(&out), unsafe.Pointer(&value), false)
	return out
}

// cloneAt clones the value at src into dst. Both point to memory of the
// shape's type, and dst holds the zero value. arc says that an `arc` tag
// covers the value.
func (s *shape) cloneAt(dst, src unsafe.Pointer, arc bool) {
	if (arc && s.arcPlain) || (!arc && s.plain) {
		s.copyAt(dst, src)
		return
	}
	if s.clone.ok {
		out := reflect.NewAt(s.typ, src).Method(s.clone.index).Call(nil)[0]
		reflect.NewAt(s.typ, dst).Elem().Set(s.cloneResult(out))
		return
	}
	switch s.kind {
	case kindPointer:
		if pointee := *(*unsafe.Pointer)(src); pointee != nil {
			*(*unsafe.Pointer)(dst) = s.elem.cloneBox(pointee)
		}
	case kindInterface:
		s.cloneInterface(dst, src)
	case kindSlice:
		source := reflect.NewAt(s.typ, src).Elem()
		if source.IsNil() {
			return
		}
		// Vec::clone allocates exactly the length, so the copy never shares
		// spare capacity with anything.
		length := source.Len()
		copied := reflect.MakeSlice(s.typ, length, length)
		if (arc && s.elem.arcPlain) || (!arc && s.elem.plain) {
			reflect.Copy(copied, source)
		} else {
			to, from := copied.UnsafePointer(), source.UnsafePointer()
			for i := range length {
				offset := uintptr(i) * s.elem.size
				s.elem.cloneAt(unsafe.Add(to, offset), unsafe.Add(from, offset), arc)
			}
		}
		reflect.NewAt(s.typ, dst).Elem().Set(copied)
	case kindArray:
		for i := range s.typ.Len() {
			offset := uintptr(i) * s.elem.size
			s.elem.cloneAt(unsafe.Add(dst, offset), unsafe.Add(src, offset), arc)
		}
	case kindGoMap:
		s.cloneGoMap(dst, src, arc)
	case kindOption:
		some := s.fields[1].offset
		if !*(*bool)(unsafe.Add(src, some)) {
			return
		}
		value := s.fields[0]
		value.shape.cloneAt(unsafe.Add(dst, value.offset), unsafe.Add(src, value.offset), arc)
		*(*bool)(unsafe.Add(dst, some)) = true
	case kindStruct, kindResult:
		for _, field := range s.fields {
			// The tag reaches through the tuples and their like, and stops
			// at any other struct, whose own tags then decide.
			field.shape.cloneAt(unsafe.Add(dst, field.offset), unsafe.Add(src, field.offset),
				field.arc || (arc && s.container))
		}
	case kindReflectMap:
		source := reflect.NewAt(s.typ, src).Interface().(ReflectMap)
		target := reflect.NewAt(s.typ, dst).Interface().(ReflectMap)
		source.ReflectEntries(func(key, value any) bool {
			target.ReflectInsert(s.key.cloneBoxed(key, arc), s.elem.cloneBoxedPointee(value, arc))
			return true
		})
	case kindReflectSeq:
		source := reflect.NewAt(s.typ, src).Interface().(ReflectSeq)
		target := reflect.NewAt(s.typ, dst).Interface().(ReflectSeq)
		source.ReflectElements(func(element any) bool {
			target.ReflectPush(s.elem.cloneBoxedPointee(element, arc))
			return true
		})
	default:
		panic("rs.Clone: unhandled kind of " + s.typ.String())
	}
}

// copyAt copies the value at src to dst as an assignment does.
func (s *shape) copyAt(dst, src unsafe.Pointer) {
	switch s.copy {
	case copyNothing:
	case copy1:
		*(*uint8)(dst) = *(*uint8)(src)
	case copy2:
		*(*uint16)(dst) = *(*uint16)(src)
	case copy4:
		*(*uint32)(dst) = *(*uint32)(src)
	case copy8:
		*(*uint64)(dst) = *(*uint64)(src)
	case copy16:
		*(*[2]uint64)(dst) = *(*[2]uint64)(src)
	case copyComplex64:
		*(*complex64)(dst) = *(*complex64)(src)
	case copyString:
		*(*string)(dst) = *(*string)(src)
	case copyPointer:
		*(*unsafe.Pointer)(dst) = *(*unsafe.Pointer)(src)
	default:
		reflect.NewAt(s.typ, dst).Elem().Set(reflect.NewAt(s.typ, src).Elem())
	}
}

// cloneResult turns what a Clone method returned into a value of the shape's
// type: the value itself, the pointee of a returned pointer, or what a
// returned interface holds.
func (s *shape) cloneResult(out reflect.Value) reflect.Value {
	for out.Kind() == reflect.Interface && !out.IsNil() {
		out = out.Elem()
	}
	if out.Type() == s.typ {
		return out
	}
	if out.Kind() == reflect.Pointer && out.Type().Elem() == s.typ && !out.IsNil() {
		return out.Elem()
	}
	panic("rs.Clone: the Clone method of " + s.typ.String() + " returned a " + out.Type().String())
}

// cloneBox is Box<T>::clone: it returns a pointer to a clone of the value at
// pointee, or pointee itself for a type that is shared.
func (s *shape) cloneBox(pointee unsafe.Pointer) unsafe.Pointer {
	if s.clone.ok {
		out := reflect.NewAt(s.typ, pointee).Method(s.clone.index).Call(nil)[0]
		for out.Kind() == reflect.Interface && !out.IsNil() {
			out = out.Elem()
		}
		if out.Kind() == reflect.Pointer && out.Type().Elem() == s.typ && !out.IsNil() {
			// A handle: its Clone returned the pointer to use.
			return out.UnsafePointer()
		}
		box := reflect.New(s.typ)
		box.Elem().Set(s.cloneResult(out))
		return box.UnsafePointer()
	}
	if s.sharedBehindPointer {
		return pointee
	}
	box := reflect.New(s.typ).UnsafePointer()
	s.cloneAt(box, pointee, false)
	return box
}

// cloneInterface clones an owned interface value.
func (s *shape) cloneInterface(dst, src unsafe.Pointer) {
	source := reflect.NewAt(s.typ, src).Elem()
	if source.IsNil() {
		return
	}
	target := reflect.NewAt(s.typ, dst).Elem()
	held := source.Elem()
	heldType := held.Type()
	if heldType.Kind() != reflect.Pointer {
		// A value in an interface (a string, a slice, a struct): cloned as
		// its own type.
		heldShape := shapeOf(heldType)
		if heldShape.plain {
			target.Set(held)
			return
		}
		from := reflect.New(heldType)
		from.Elem().Set(held)
		to := reflect.New(heldType)
		heldShape.cloneAt(to.UnsafePointer(), from.UnsafePointer(), false)
		target.Set(to.Elem())
		return
	}
	if held.IsNil() || !(s.sealed || enumVariantInfo(heldType.Elem()) != nil) {
		// A trait object: Arc<dyn Trait> shares.
		target.Set(held)
		return
	}
	variant := shapeOf(heldType.Elem())
	target.Set(reflect.NewAt(variant.typ, variant.cloneBox(held.UnsafePointer())))
}

func (s *shape) cloneGoMap(dst, src unsafe.Pointer, arc bool) {
	source := reflect.NewAt(s.typ, src).Elem()
	if source.IsNil() {
		return
	}
	copied := reflect.MakeMapWithSize(s.typ, source.Len())
	valuePlain := (arc && s.elem.arcPlain) || (!arc && s.elem.plain)
	var from, to reflect.Value
	if !valuePlain {
		from, to = reflect.New(s.elem.typ), reflect.New(s.elem.typ)
	}
	entries := source.MapRange()
	for entries.Next() {
		// A key is copied as it is: a Go map finds an entry by the key's own
		// identity.
		if valuePlain {
			copied.SetMapIndex(entries.Key(), entries.Value())
			continue
		}
		from.Elem().SetIterValue(entries)
		to.Elem().SetZero()
		s.elem.cloneAt(to.UnsafePointer(), from.UnsafePointer(), arc)
		copied.SetMapIndex(entries.Key(), to.Elem())
	}
	reflect.NewAt(s.typ, dst).Elem().Set(copied)
}

// cloneBoxed clones a value of the shape's type that a collection handed
// over in an interface.
func (s *shape) cloneBoxed(value any, arc bool) any {
	if (arc && s.arcPlain) || (!arc && s.plain) {
		return value
	}
	from := reflect.New(s.typ)
	if value != nil {
		from.Elem().Set(reflect.ValueOf(value))
	}
	to := reflect.New(s.typ)
	s.cloneAt(to.UnsafePointer(), from.UnsafePointer(), arc)
	return to.Elem().Interface()
}

// cloneBoxedPointee clones the value that a *T handed over in an interface
// points to, and returns it as a T.
func (s *shape) cloneBoxedPointee(pointer any, arc bool) any {
	from := reflect.ValueOf(pointer)
	if from.Kind() != reflect.Pointer || from.Type().Elem() != s.typ {
		panic("rs.Clone: a collection of " + s.typ.String() + " yielded a " + from.Type().String() +
			" where ReflectEntries and ReflectElements yield a pointer to the element")
	}
	if (arc && s.arcPlain) || (!arc && s.plain) {
		return from.Elem().Interface()
	}
	to := reflect.New(s.typ)
	s.cloneAt(to.UnsafePointer(), from.UnsafePointer(), arc)
	return to.Elem().Interface()
}

// arcPointee returns the address of the value that the rs.Arc at p holds.
func (s *shape) arcPointee(p unsafe.Pointer) unsafe.Pointer {
	return reflect.NewAt(s.typ, p).Interface().(arcValue).arcPointee()
}

// variantInfos caches the registry entry of each variant struct that has
// one. Only hits are kept: a type may be registered after it was first seen.
var variantInfos sync.Map

// enumVariantInfo returns the registration of t when t is the struct of an
// enum variant, and nil otherwise.
func enumVariantInfo(t reflect.Type) *TypeInfo {
	if info, ok := variantInfos.Load(t); ok {
		return info.(*TypeInfo)
	}
	info := TypeInfoOf(t)
	if info == nil || info.Enum == nil {
		return nil
	}
	variantInfos.Store(t, info)
	return info
}
