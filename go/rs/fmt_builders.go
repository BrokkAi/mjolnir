// Port of core 1.99.1 src/fmt/builders.rs

package rs

import (
	"reflect"
	"strings"
)

// fmtPadAdapter is builders::PadAdapter: it indents what passes through it by
// four spaces after every newline, which is the whole of `{:#?}`'s layout.
type fmtPadAdapter struct {
	buf   fmtWrite
	state *fmtPadAdapterState
}

type fmtPadAdapterState struct {
	onNewline bool
}

func fmtPadAdapterStateDefault() fmtPadAdapterState {
	return fmtPadAdapterState{onNewline: true}
}

// fmtPadAdapterWrap is PadAdapter::wrap.
func fmtPadAdapterWrap(f *Formatter, state *fmtPadAdapterState) *Formatter {
	return f.wrapBuf(&fmtPadAdapter{buf: f.buf, state: state})
}

func (p *fmtPadAdapter) writeStr(s string) error {
	// for s in s.split_inclusive('\n')
	for len(s) > 0 {
		piece := s
		if newline := strings.IndexByte(s, '\n'); newline >= 0 {
			piece = s[:newline+1]
		}
		s = s[len(piece):]

		if p.state.onNewline {
			if err := p.buf.writeStr("    "); err != nil {
				return err
			}
		}

		p.state.onNewline = strings.HasSuffix(piece, "\n")
		if err := p.buf.writeStr(piece); err != nil {
			return err
		}
	}

	return nil
}

func (p *fmtPadAdapter) writeChar(c rune) error {
	if p.state.onNewline {
		if err := p.buf.writeStr("    "); err != nil {
			return err
		}
	}
	p.state.onNewline = c == '\n'
	return p.buf.writeChar(c)
}

// fmtDebugOnce is builders::DebugOnce: a value whose Debug output is what a
// function writes.
type fmtDebugOnce func(f *Formatter) error

// FmtDebug runs the function.
func (once fmtDebugOnce) FmtDebug(f *Formatter) error { return once(f) }

// DebugStruct is Rust's fmt::DebugStruct builder, which a hand-written
// FmtDebug uses to print `Name { field: value }`.
type DebugStruct struct {
	fmt       *Formatter
	result    error
	hasFields bool
}

func fmtDebugStructNew(f *Formatter, name string) *DebugStruct {
	result := f.WriteStr(name)
	return &DebugStruct{fmt: f, result: result, hasFields: false}
}

// Field is DebugStruct::field: it adds a field, printing value with Debug.
func (b *DebugStruct) Field(name string, value any) *DebugStruct {
	if b.result == nil {
		b.result = func() error {
			if b.isPretty() {
				if !b.hasFields {
					if err := b.fmt.WriteStr(" {\n"); err != nil {
						return err
					}
				}
				state := fmtPadAdapterStateDefault()
				writer := fmtPadAdapterWrap(b.fmt, &state)
				if err := writer.WriteStr(name); err != nil {
					return err
				}
				if err := writer.WriteStr(": "); err != nil {
					return err
				}
				if err := DebugFmt(value, writer); err != nil {
					return err
				}
				return writer.WriteStr(",\n")
			}
			prefix := " { "
			if b.hasFields {
				prefix = ", "
			}
			if err := b.fmt.WriteStr(prefix); err != nil {
				return err
			}
			if err := b.fmt.WriteStr(name); err != nil {
				return err
			}
			if err := b.fmt.WriteStr(": "); err != nil {
				return err
			}
			return DebugFmt(value, b.fmt)
		}()
	}

	b.hasFields = true
	return b
}

// fieldWith is DebugStruct::field_with: a field whose value is written by a
// function.
func (b *DebugStruct) fieldWith(name string, valueFmt func(f *Formatter) error) *DebugStruct {
	return b.Field(name, fmtDebugOnce(valueFmt))
}

// FinishNonExhaustive is DebugStruct::finish_non_exhaustive: it marks the
// struct as having fields that are not shown.
func (b *DebugStruct) FinishNonExhaustive() error {
	if b.result == nil {
		b.result = func() error {
			if b.hasFields {
				if b.isPretty() {
					state := fmtPadAdapterStateDefault()
					writer := fmtPadAdapterWrap(b.fmt, &state)
					if err := writer.WriteStr("..\n"); err != nil {
						return err
					}
					return b.fmt.WriteStr("}")
				}
				return b.fmt.WriteStr(", .. }")
			}
			return b.fmt.WriteStr(" { .. }")
		}()
	}
	return b.result
}

// Finish is DebugStruct::finish.
func (b *DebugStruct) Finish() error {
	if b.hasFields {
		if b.result == nil {
			if b.isPretty() {
				b.result = b.fmt.WriteStr("}")
			} else {
				b.result = b.fmt.WriteStr(" }")
			}
		}
	}
	return b.result
}

func (b *DebugStruct) isPretty() bool {
	return b.fmt.Alternate()
}

// DebugTuple is Rust's fmt::DebugTuple builder, which prints `Name(value,
// value)`.
type DebugTuple struct {
	fmt       *Formatter
	result    error
	fields    int
	emptyName bool
}

func fmtDebugTupleNew(f *Formatter, name string) *DebugTuple {
	result := f.WriteStr(name)
	return &DebugTuple{fmt: f, result: result, fields: 0, emptyName: name == ""}
}

// Field is DebugTuple::field: it adds a field, printing value with Debug.
func (b *DebugTuple) Field(value any) *DebugTuple {
	if b.result == nil {
		b.result = func() error {
			if b.isPretty() {
				if b.fields == 0 {
					if err := b.fmt.WriteStr("(\n"); err != nil {
						return err
					}
				}
				state := fmtPadAdapterStateDefault()
				writer := fmtPadAdapterWrap(b.fmt, &state)
				if err := DebugFmt(value, writer); err != nil {
					return err
				}
				return writer.WriteStr(",\n")
			}
			prefix := ", "
			if b.fields == 0 {
				prefix = "("
			}
			if err := b.fmt.WriteStr(prefix); err != nil {
				return err
			}
			return DebugFmt(value, b.fmt)
		}()
	}

	b.fields++
	return b
}

// fieldWith is DebugTuple::field_with: a field whose value is written by a
// function.
func (b *DebugTuple) fieldWith(valueFmt func(f *Formatter) error) *DebugTuple {
	return b.Field(fmtDebugOnce(valueFmt))
}

// FinishNonExhaustive is DebugTuple::finish_non_exhaustive.
func (b *DebugTuple) FinishNonExhaustive() error {
	if b.result == nil {
		b.result = func() error {
			if b.fields > 0 {
				if b.isPretty() {
					state := fmtPadAdapterStateDefault()
					writer := fmtPadAdapterWrap(b.fmt, &state)
					if err := writer.WriteStr("..\n"); err != nil {
						return err
					}
					return b.fmt.WriteStr(")")
				}
				return b.fmt.WriteStr(", ..)")
			}
			return b.fmt.WriteStr("(..)")
		}()
	}
	return b.result
}

// Finish is DebugTuple::finish.
func (b *DebugTuple) Finish() error {
	if b.fields > 0 {
		if b.result == nil {
			b.result = func() error {
				if b.fields == 1 && b.emptyName && !b.isPretty() {
					if err := b.fmt.WriteStr(","); err != nil {
						return err
					}
				}
				return b.fmt.WriteStr(")")
			}()
		}
	}
	return b.result
}

func (b *DebugTuple) isPretty() bool {
	return b.fmt.Alternate()
}

// fmtDebugInner is builders::DebugInner, shared by DebugSet and DebugList.
type fmtDebugInner struct {
	fmt       *Formatter
	result    error
	hasFields bool
}

func (b *fmtDebugInner) entry(entry any) {
	if b.result == nil {
		b.result = func() error {
			if b.isPretty() {
				if !b.hasFields {
					if err := b.fmt.WriteStr("\n"); err != nil {
						return err
					}
				}
				state := fmtPadAdapterStateDefault()
				writer := fmtPadAdapterWrap(b.fmt, &state)
				if err := DebugFmt(entry, writer); err != nil {
					return err
				}
				return writer.WriteStr(",\n")
			}
			if b.hasFields {
				if err := b.fmt.WriteStr(", "); err != nil {
					return err
				}
			}
			return DebugFmt(entry, b.fmt)
		}()
	}

	b.hasFields = true
}

// entryWith is DebugInner::entry_with: an entry written by a function.
func (b *fmtDebugInner) entryWith(entryFmt func(f *Formatter) error) {
	b.entry(fmtDebugOnce(entryFmt))
}

// entries adds each element of a slice, an array or a collection that
// implements ReflectSeq.
func (b *fmtDebugInner) entries(values any) {
	fmtEachElement(values, func(element any) {
		b.entry(element)
	})
}

func (b *fmtDebugInner) isPretty() bool {
	return b.fmt.Alternate()
}

// finishNonExhaustive is the body shared by DebugSet and DebugList's
// finish_non_exhaustive; closing is `}` or `]`.
func (b *fmtDebugInner) finishNonExhaustive(closing string) error {
	if b.hasFields {
		if b.isPretty() {
			state := fmtPadAdapterStateDefault()
			writer := fmtPadAdapterWrap(b.fmt, &state)
			if err := writer.WriteStr("..\n"); err != nil {
				return err
			}
			return b.fmt.WriteStr(closing)
		}
		return b.fmt.WriteStr(", .." + closing)
	}
	return b.fmt.WriteStr(".." + closing)
}

// DebugSet is Rust's fmt::DebugSet builder, which prints `{value, value}`.
type DebugSet struct {
	inner fmtDebugInner
}

func fmtDebugSetNew(f *Formatter) *DebugSet {
	result := f.WriteStr("{")
	return &DebugSet{inner: fmtDebugInner{fmt: f, result: result, hasFields: false}}
}

// Entry is DebugSet::entry: it adds an element, printing it with Debug.
func (b *DebugSet) Entry(value any) *DebugSet {
	b.inner.entry(value)
	return b
}

// Entries is DebugSet::entries over a slice, an array, or a collection that
// implements ReflectSeq.
func (b *DebugSet) Entries(values any) *DebugSet {
	b.inner.entries(values)
	return b
}

// FinishNonExhaustive is DebugSet::finish_non_exhaustive.
func (b *DebugSet) FinishNonExhaustive() error {
	if b.inner.result == nil {
		b.inner.result = b.inner.finishNonExhaustive("}")
	}
	return b.inner.result
}

// Finish is DebugSet::finish.
func (b *DebugSet) Finish() error {
	if b.inner.result == nil {
		b.inner.result = b.inner.fmt.WriteStr("}")
	}
	return b.inner.result
}

// DebugList is Rust's fmt::DebugList builder, which prints `[value, value]`.
type DebugList struct {
	inner fmtDebugInner
}

func fmtDebugListNew(f *Formatter) *DebugList {
	result := f.WriteStr("[")
	return &DebugList{inner: fmtDebugInner{fmt: f, result: result, hasFields: false}}
}

// Entry is DebugList::entry: it adds an element, printing it with Debug.
func (b *DebugList) Entry(value any) *DebugList {
	b.inner.entry(value)
	return b
}

// Entries is DebugList::entries over a slice, an array, or a collection that
// implements ReflectSeq.
func (b *DebugList) Entries(values any) *DebugList {
	b.inner.entries(values)
	return b
}

// FinishNonExhaustive is DebugList::finish_non_exhaustive.
func (b *DebugList) FinishNonExhaustive() error {
	// Rust does not store this result in the builder; nothing can be added
	// after it either way.
	if b.inner.result != nil {
		return b.inner.result
	}
	return b.inner.finishNonExhaustive("]")
}

// Finish is DebugList::finish.
func (b *DebugList) Finish() error {
	if b.inner.result == nil {
		b.inner.result = b.inner.fmt.WriteStr("]")
	}
	return b.inner.result
}

// DebugMap is Rust's fmt::DebugMap builder, which prints `{key: value, key:
// value}`.
type DebugMap struct {
	fmt       *Formatter
	result    error
	hasFields bool
	hasKey    bool
	// The state of newlines is tracked between keys and values
	state fmtPadAdapterState
}

func fmtDebugMapNew(f *Formatter) *DebugMap {
	result := f.WriteStr("{")
	return &DebugMap{fmt: f, result: result, hasFields: false, hasKey: false, state: fmtPadAdapterStateDefault()}
}

// Entry is DebugMap::entry: it adds a key and its value, printing both with
// Debug.
func (b *DebugMap) Entry(key, value any) *DebugMap {
	return b.Key(key).Value(value)
}

// Key is DebugMap::key: the key half of an entry, to be followed by Value.
func (b *DebugMap) Key(key any) *DebugMap {
	if b.result == nil {
		b.result = func() error {
			if b.hasKey {
				panic(fmtPanicInfo("attempted to begin a new map entry without completing the previous one"))
			}

			if b.isPretty() {
				if !b.hasFields {
					if err := b.fmt.WriteStr("\n"); err != nil {
						return err
					}
				}
				b.state = fmtPadAdapterStateDefault()
				writer := fmtPadAdapterWrap(b.fmt, &b.state)
				if err := DebugFmt(key, writer); err != nil {
					return err
				}
				if err := writer.WriteStr(": "); err != nil {
					return err
				}
			} else {
				if b.hasFields {
					if err := b.fmt.WriteStr(", "); err != nil {
						return err
					}
				}
				if err := DebugFmt(key, b.fmt); err != nil {
					return err
				}
				if err := b.fmt.WriteStr(": "); err != nil {
					return err
				}
			}

			b.hasKey = true
			return nil
		}()
	}

	return b
}

// keyWith is DebugMap::key_with: a key written by a function.
func (b *DebugMap) keyWith(keyFmt func(f *Formatter) error) *DebugMap {
	return b.Key(fmtDebugOnce(keyFmt))
}

// Value is DebugMap::value: the value half of an entry begun with Key.
func (b *DebugMap) Value(value any) *DebugMap {
	if b.result == nil {
		b.result = func() error {
			if !b.hasKey {
				panic(fmtPanicInfo("attempted to format a map value before its key"))
			}

			if b.isPretty() {
				writer := fmtPadAdapterWrap(b.fmt, &b.state)
				if err := DebugFmt(value, writer); err != nil {
					return err
				}
				if err := writer.WriteStr(",\n"); err != nil {
					return err
				}
			} else {
				if err := DebugFmt(value, b.fmt); err != nil {
					return err
				}
			}

			b.hasKey = false
			return nil
		}()
	}

	b.hasFields = true
	return b
}

// valueWith is DebugMap::value_with: a value written by a function.
func (b *DebugMap) valueWith(valueFmt func(f *Formatter) error) *DebugMap {
	return b.Value(fmtDebugOnce(valueFmt))
}

// Entries is DebugMap::entries over a Go map (in sorted key order) or a
// collection that implements ReflectMap.
func (b *DebugMap) Entries(entries any) *DebugMap {
	fmtEachEntry(entries, func(key, value any) {
		b.Entry(key, value)
	})
	return b
}

// FinishNonExhaustive is DebugMap::finish_non_exhaustive.
func (b *DebugMap) FinishNonExhaustive() error {
	if b.result == nil {
		b.result = func() error {
			if b.hasKey {
				panic(fmtPanicInfo("attempted to finish a map with a partial entry"))
			}

			if b.hasFields {
				if b.isPretty() {
					state := fmtPadAdapterStateDefault()
					writer := fmtPadAdapterWrap(b.fmt, &state)
					if err := writer.WriteStr("..\n"); err != nil {
						return err
					}
					return b.fmt.WriteStr("}")
				}
				return b.fmt.WriteStr(", ..}")
			}
			return b.fmt.WriteStr("..}")
		}()
	}
	return b.result
}

// Finish is DebugMap::finish.
func (b *DebugMap) Finish() error {
	if b.result == nil {
		b.result = func() error {
			if b.hasKey {
				panic(fmtPanicInfo("attempted to finish a map with a partial entry"))
			}

			return b.fmt.WriteStr("}")
		}()
	}
	return b.result
}

func (b *DebugMap) isPretty() bool {
	return b.fmt.Alternate()
}

// FmtFromFn is Rust's fmt::FromFn: a value whose Display and Debug output is
// whatever the function writes.
type FmtFromFn func(f *Formatter) error

// FmtFromFnOf is fmt::from_fn.
func FmtFromFnOf(f func(f *Formatter) error) FmtFromFn { return FmtFromFn(f) }

// FmtDebug runs the function.
func (fn FmtFromFn) FmtDebug(f *Formatter) error { return fn(f) }

// FmtDisplay runs the function.
func (fn FmtFromFn) FmtDisplay(f *Formatter) error { return fn(f) }

// fmtEachElement calls yield with every element of a slice, an array, or a
// collection that implements ReflectSeq, in order.
func fmtEachElement(values any, yield func(element any)) {
	if seq, ok := values.(ReflectSeq); ok {
		fmtSeqElements(seq, yield)
		return
	}
	v := reflect.ValueOf(values)
	for v.Kind() == reflect.Pointer && !v.IsNil() {
		if seq, ok := v.Interface().(ReflectSeq); ok {
			fmtSeqElements(seq, yield)
			return
		}
		v = v.Elem()
	}
	switch v.Kind() {
	case reflect.Slice, reflect.Array:
		for i := 0; i < v.Len(); i++ {
			yield(v.Index(i).Interface())
		}
	case reflect.Struct:
		if v.CanAddr() {
			if seq, ok := v.Addr().Interface().(ReflectSeq); ok {
				fmtSeqElements(seq, yield)
				return
			}
		}
		fallthrough
	default:
		panic(fmtPanicInfo("rs: Entries needs a slice, an array or a collection that implements rs.ReflectSeq, not " + fmtGoTypeName(v)))
	}
}

// fmtEachEntry calls yield with every entry of a Go map (in sorted key order)
// or a collection that implements ReflectMap.
func fmtEachEntry(entries any, yield func(key, value any)) {
	if m, ok := entries.(ReflectMap); ok {
		fmtMapEntries(m, yield)
		return
	}
	v := reflect.ValueOf(entries)
	for v.Kind() == reflect.Pointer && !v.IsNil() {
		if m, ok := v.Interface().(ReflectMap); ok {
			fmtMapEntries(m, yield)
			return
		}
		v = v.Elem()
	}
	switch v.Kind() {
	case reflect.Map:
		for _, key := range fmtSortedMapKeys(v) {
			yield(key.Interface(), v.MapIndex(key).Interface())
		}
	case reflect.Struct:
		if v.CanAddr() {
			if m, ok := v.Addr().Interface().(ReflectMap); ok {
				fmtMapEntries(m, yield)
				return
			}
		}
		fallthrough
	default:
		panic(fmtPanicInfo("rs: Entries needs a map or a collection that implements rs.ReflectMap, not " + fmtGoTypeName(v)))
	}
}
