// Port of serde_derive 1.0.229 src/de.rs and src/de/*.rs: the Deserialize
// impl that `#[derive(Deserialize)]` generates, run over the model in
// derive.go instead of generated as code.

package serde

import (
	"reflect"
	"strconv"
	"strings"
	"unsafe"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// identKey is the value of a generated `__Field` identifier: the position
// of a field or variant in the identifier's list, identIgnore for
// `__ignore`, or a buffered key for `__other(Content)`.
type identKey struct {
	index int
	other *content
}

const identIgnore = -1

// fallthroughKind is what an identifier does with a name it does not know.
type fallthroughKind uint8

const (
	// fallthroughError reports unknown_field or unknown_variant.
	fallthroughError fallthroughKind = iota
	// fallthroughIgnore is `__ignore`.
	fallthroughIgnore
	// fallthroughOther is a `#[serde(other)]` variant, at otherIndex.
	fallthroughOther
	// fallthroughCollect is `__other(Content)`, for a struct with a
	// flattened field.
	fallthroughCollect
)

// identifier is a generated `__FieldVisitor` (`deserialize_identifier`).
type identifier struct {
	// aliases holds, for each entry, the names that select it.
	aliases [][]string
	// names is the `FIELDS` or `VARIANTS` constant of the error messages.
	names      []string
	isVariant  bool
	fallback   fallthroughKind
	otherIndex int
	expecting  string
}

func (id *identifier) lookup(value string) (int, bool) {
	for index, aliases := range id.aliases {
		for _, alias := range aliases {
			if alias == value {
				return index, true
			}
		}
	}
	return 0, false
}

// visitor returns the identifier's visitor: the one with the methods for
// collecting the keys a flattened field may take, when it collects.
func (id *identifier) visitor() DeVisitor {
	if id.fallback == fallthroughCollect {
		return &identCollectVisitor{identVisitor{id: id}}
	}
	return &identVisitor{id: id}
}

// DeserializeSeed is `__Field::deserialize`.
func (id *identifier) DeserializeSeed(deserializer Deserializer) (any, error) {
	return deserializer.DeserializeIdentifier(id.visitor())
}

func (id *identifier) unknown(value string) (any, error) {
	switch id.fallback {
	case fallthroughIgnore:
		return identKey{index: identIgnore}, nil
	case fallthroughOther:
		return identKey{index: id.otherIndex}, nil
	}
	if id.isVariant {
		return nil, DeErrorUnknownVariant(value, id.names)
	}
	return nil, DeErrorUnknownField(value, id.names)
}

type identVisitor struct{ id *identifier }

func (v *identVisitor) Expecting() string { return v.id.expecting }

func (v *identVisitor) VisitU64(value uint64) (any, error) {
	if value < uint64(len(v.id.aliases)) {
		return identKey{index: int(value)}, nil
	}
	switch v.id.fallback {
	case fallthroughIgnore:
		return identKey{index: identIgnore}, nil
	case fallthroughOther:
		return identKey{index: v.id.otherIndex}, nil
	}
	kind := "field"
	if v.id.isVariant {
		kind = "variant"
	}
	return nil, DeErrorInvalidValue(&DeUnexpectedUnsigned{F0: value}, DeExpectedStr(kind+" index 0 <= i < "+strconv.Itoa(len(v.id.aliases))))
}

func (v *identVisitor) VisitStr(value string) (any, error) {
	if index, ok := v.id.lookup(value); ok {
		return identKey{index: index}, nil
	}
	return v.id.unknown(value)
}

func (v *identVisitor) VisitBytes(value []byte) (any, error) {
	if index, ok := v.id.lookup(string(value)); ok {
		return identKey{index: index}, nil
	}
	if v.id.fallback == fallthroughError {
		return v.id.unknown(fromUtf8Lossy(value))
	}
	return v.id.unknown(string(value))
}

// identCollectVisitor is the identifier of a struct with a flattened field:
// every key it does not know is kept as `__other(Content)`.
type identCollectVisitor struct{ identVisitor }

func other(c content) (any, error) { return identKey{index: identIgnore, other: &c}, nil }

func (v *identCollectVisitor) VisitBool(value bool) (any, error) {
	if value {
		return other(content{kind: contentBool, num: 1})
	}
	return other(content{kind: contentBool})
}
func (v *identCollectVisitor) VisitI8(value int8) (any, error) {
	return other(content{kind: contentI8, num: uint64(int64(value))})
}
func (v *identCollectVisitor) VisitI16(value int16) (any, error) {
	return other(content{kind: contentI16, num: uint64(int64(value))})
}
func (v *identCollectVisitor) VisitI32(value int32) (any, error) {
	return other(content{kind: contentI32, num: uint64(int64(value))})
}
func (v *identCollectVisitor) VisitI64(value int64) (any, error) {
	return other(content{kind: contentI64, num: uint64(value)})
}
func (v *identCollectVisitor) VisitU8(value uint8) (any, error) {
	return other(content{kind: contentU8, num: uint64(value)})
}
func (v *identCollectVisitor) VisitU16(value uint16) (any, error) {
	return other(content{kind: contentU16, num: uint64(value)})
}
func (v *identCollectVisitor) VisitU32(value uint32) (any, error) {
	return other(content{kind: contentU32, num: uint64(value)})
}
func (v *identCollectVisitor) VisitU64(value uint64) (any, error) {
	return other(content{kind: contentU64, num: value})
}
func (v *identCollectVisitor) VisitF32(value float32) (any, error) {
	var c content
	_, _ = contentVisitor{out: &c}.VisitF32(value)
	return other(c)
}
func (v *identCollectVisitor) VisitF64(value float64) (any, error) {
	var c content
	_, _ = contentVisitor{out: &c}.VisitF64(value)
	return other(c)
}
func (v *identCollectVisitor) VisitChar(value rune) (any, error) {
	return other(content{kind: contentChar, num: uint64(value)})
}
func (v *identCollectVisitor) VisitUnit() (any, error) { return other(content{kind: contentUnit}) }

func (v *identCollectVisitor) VisitStr(value string) (any, error) {
	if index, ok := v.id.lookup(value); ok {
		return identKey{index: index}, nil
	}
	return other(content{kind: contentString, str: value})
}

func (v *identCollectVisitor) VisitBorrowedStr(value string) (any, error) {
	if index, ok := v.id.lookup(value); ok {
		return identKey{index: index}, nil
	}
	return other(content{kind: contentStr, str: value})
}

func (v *identCollectVisitor) VisitBytes(value []byte) (any, error) {
	if index, ok := v.id.lookup(string(value)); ok {
		return identKey{index: index}, nil
	}
	return other(content{kind: contentByteBuf, bytes: append([]byte(nil), value...)})
}

func (v *identCollectVisitor) VisitBorrowedBytes(value []byte) (any, error) {
	if index, ok := v.id.lookup(string(value)); ok {
		return identKey{index: index}, nil
	}
	return other(content{kind: contentBytes, bytes: value})
}

// fromUtf8Lossy is `String::from_utf8_lossy`: each invalid sequence becomes
// one U+FFFD, as Rust splits them.
func fromUtf8Lossy(value []byte) string {
	var out strings.Builder
	for len(value) > 0 {
		valid, invalid, rest := utf8Chunk(value)
		out.Write(valid)
		if invalid {
			out.WriteString("�")
		}
		value = rest
	}
	return out.String()
}

// utf8Chunk is one step of `Utf8Chunks`: the valid prefix, whether an
// invalid sequence follows it, and what comes after that sequence.
func utf8Chunk(value []byte) (valid []byte, invalid bool, rest []byte) {
	i := 0
	for i < len(value) {
		b := value[i]
		if b < 0x80 {
			i++
			continue
		}
		width := 0
		switch {
		case b >= 0xC2 && b <= 0xDF:
			width = 2
		case b >= 0xE0 && b <= 0xEF:
			width = 3
		case b >= 0xF0 && b <= 0xF4:
			width = 4
		default:
			return value[:i], true, value[i+1:]
		}
		// Check the continuation bytes as `run_utf8_validation` does, and
		// count how many belong to the invalid sequence.
		j := i + 1
		for k := 1; k < width; k++ {
			if j >= len(value) {
				return value[:i], true, value[j:]
			}
			c := value[j]
			lo, hi := byte(0x80), byte(0xBF)
			if k == 1 {
				switch b {
				case 0xE0:
					lo = 0xA0
				case 0xED:
					hi = 0x9F
				case 0xF0:
					lo = 0x90
				case 0xF4:
					hi = 0x8F
				}
			}
			if c < lo || c > hi {
				return value[:i], true, value[j:]
			}
			j++
		}
		i = j
	}
	return value, false, nil
}

// fieldIdentifier is `deserialize_field_identifier` for a set of fields.
func (fs *fieldSet) identifier() *identifier {
	id := &identifier{names: fs.fieldNames, expecting: "field identifier"}
	for _, f := range fs.deFields {
		id.aliases = append(id.aliases, f.attrs.name.aliases)
	}
	switch {
	case fs.hasFlattenDe():
		id.fallback = fallthroughCollect
	case fs.cont.denyUnknownFields:
		id.fallback = fallthroughError
	default:
		id.fallback = fallthroughIgnore
	}
	return id
}

// structForm is `StructForm` and `TupleForm`: where the fields being read
// belong.
type structForm uint8

const (
	formStruct structForm = iota
	formExternallyTagged
	formInternallyTagged
	formUntagged
)

// fieldsVisitor is the generated `__Visitor` of a struct, a tuple struct or
// a struct or tuple variant. It builds the value at base.
type fieldsVisitor struct {
	fs        *fieldSet
	base      unsafe.Pointer
	expecting string
	id        *identifier
}

func (v *fieldsVisitor) Expecting() string { return v.expecting }

// fieldsSeqVisitor adds `visit_seq`, which every form has except an
// untagged struct variant and a struct with a flattened field.
type fieldsSeqVisitor struct{ fieldsVisitor }

func (v *fieldsSeqVisitor) VisitSeq(seq DeSeqAccess) (any, error) {
	return nil, v.fs.deserializeSeq(seq, v.base, v.expecting)
}

func (v *fieldsVisitor) VisitMap(m DeMapAccess) (any, error) {
	return nil, v.fs.deserializeMap(m, v.base, v.id)
}

// tupleFieldsVisitor is the `__Visitor` of a tuple struct or tuple
// variant: only visit_seq.
type tupleFieldsVisitor struct {
	fs        *fieldSet
	base      unsafe.Pointer
	expecting string
}

func (v *tupleFieldsVisitor) Expecting() string { return v.expecting }

func (v *tupleFieldsVisitor) VisitSeq(seq DeSeqAccess) (any, error) {
	return nil, v.fs.deserializeSeq(seq, v.base, v.expecting)
}

// newtypeVisitor is the `__Visitor` of a newtype struct: it also has
// `visit_newtype_struct`.
type newtypeVisitor struct{ tupleFieldsVisitor }

func (v *newtypeVisitor) VisitNewtypeStruct(deserializer Deserializer) (any, error) {
	return nil, v.fs.fields[0].deserialize(v.base, deserializer)
}

// missing is `expr_is_missing`: the value of a field that the input did not
// have, stored at its place in base. defaults is the container's Default,
// built once (`__default`).
func (fs *fieldSet) missing(f *field, base unsafe.Pointer, defaults func() unsafe.Pointer) error {
	switch f.attrs.dflt.kind {
	case defaultDefault, defaultPath:
		f.setDefault(f.pointer(base))
		return nil
	}
	if fs.cont.dflt.kind != defaultNone {
		copyField(f, base, defaults())
		return nil
	}
	if f.deWith != nil {
		return DeErrorMissingField(f.attrs.name.deserialize)
	}
	// missing_field: an Option is None; anything else is an error.
	return f.codec().de(f.pointer(base), missingFieldDeserializer{field: f.attrs.name.deserialize})
}

// copyField copies the field f from the value at from to the value at to.
func copyField(f *field, to unsafe.Pointer, from unsafe.Pointer) {
	reflect.NewAt(f.typ, f.pointer(to)).Elem().Set(reflect.NewAt(f.typ, f.pointer(from)).Elem())
}

// containerDefaults returns a function that builds `__default` on first
// use: the container's Default, or nil when it has none.
func (fs *fieldSet) containerDefaults() func() unsafe.Pointer {
	var built unsafe.Pointer
	return func() unsafe.Pointer {
		if built == nil {
			value := reflect.New(fs.typ)
			fs.containerDefault(value.UnsafePointer())
			built = value.UnsafePointer()
		}
		return built
	}
}

// deserializeSeq is `deserialize_seq`.
func (fs *fieldSet) deserializeSeq(seq DeSeqAccess, base unsafe.Pointer, expecting string) error {
	deserializedCount := 0
	for _, f := range fs.fields {
		if !f.attrs.skipDeserializing {
			deserializedCount++
		}
	}
	lengthExpecting := expecting + " with " + strconv.Itoa(deserializedCount) + " elements"
	if deserializedCount == 1 {
		lengthExpecting = expecting + " with 1 element"
	}
	if fs.cont.hasExpecting {
		lengthExpecting = fs.cont.expecting
	}
	// let __default: Self::Value = ...;
	var defaults func() unsafe.Pointer
	if fs.cont.dflt.kind != defaultNone {
		value := reflect.New(fs.typ)
		fs.containerDefault(value.UnsafePointer())
		defaults = func() unsafe.Pointer { return value.UnsafePointer() }
	}
	indexInSeq := 0
	for _, f := range fs.fields {
		if f.attrs.skipDeserializing {
			if err := fs.missing(f, base, defaults); err != nil {
				return err
			}
			continue
		}
		_, ok, err := seq.NextElementSeed(f.seed(base))
		if err != nil {
			return err
		}
		if !ok {
			switch {
			case f.attrs.dflt.kind != defaultNone:
				f.setDefault(f.pointer(base))
			case fs.cont.dflt.kind != defaultNone:
				copyField(f, base, defaults())
			default:
				return DeErrorInvalidLength(indexInSeq, DeExpectedStr(lengthExpecting))
			}
		}
		indexInSeq++
	}
	return nil
}

// deserializeMap is `deserialize_map`.
func (fs *fieldSet) deserializeMap(m DeMapAccess, base unsafe.Pointer, id *identifier) error {
	seen := make([]bool, len(fs.deFields))
	hasFlatten := fs.hasFlattenDe()
	var collect []flatMapEntry
	for {
		key, ok, err := m.NextKeySeed(id)
		if err != nil {
			return err
		}
		if !ok {
			break
		}
		k := key.(identKey)
		switch {
		case k.other != nil:
			entry := flatMapEntry{present: true, key: *k.other}
			if _, err := m.NextValueSeed(contentVisitor{out: &entry.value}); err != nil {
				return err
			}
			collect = append(collect, entry)
		case k.index == identIgnore:
			if _, err := m.NextValueSeed(DeIgnoredAny{}); err != nil {
				return err
			}
		default:
			f := fs.deFields[k.index]
			if seen[k.index] {
				return DeErrorDuplicateField(f.attrs.name.deserialize)
			}
			if _, err := m.NextValueSeed(f.seed(base)); err != nil {
				return err
			}
			seen[k.index] = true
		}
	}
	defaults := fs.containerDefaults()
	for index, f := range fs.deFields {
		if seen[index] {
			continue
		}
		if err := fs.missing(f, base, defaults); err != nil {
			return err
		}
	}
	for _, f := range fs.fields {
		if !f.attrs.flatten || f.attrs.skipDeserializing {
			continue
		}
		if err := f.deserialize(base, flatMapDeserializer{entries: &collect}); err != nil {
			return err
		}
	}
	if hasFlatten && fs.cont.denyUnknownFields {
		for index := range collect {
			if !collect[index].present {
				continue
			}
			if key, ok := contentAsStr(&collect[index].key); ok {
				return DeErrorCustom("unknown field `" + key + "`")
			}
			return DeErrorCustom("unexpected map key")
		}
	}
	for _, f := range fs.fields {
		if f.attrs.skipDeserializing {
			if err := fs.missing(f, base, defaults); err != nil {
				return err
			}
		}
	}
	return nil
}

// structVisitorFor is the `__Visitor` that `struct_::deserialize` makes for
// form, and the expecting text it reports.
func (fs *fieldSet) structVisitorFor(form structForm, variantIdent string, base unsafe.Pointer) *fieldsSeqVisitor {
	expecting := "struct " + fs.typeName
	if form != formStruct {
		expecting = "struct variant " + fs.typeName + "::" + variantIdent
	}
	if fs.cont.hasExpecting {
		expecting = fs.cont.expecting
	}
	return &fieldsSeqVisitor{fieldsVisitor{fs: fs, base: base, expecting: expecting, id: fs.identifier()}}
}

// hasVisitSeq is whether the struct visitor of form has visit_seq.
func (fs *fieldSet) hasVisitSeq(form structForm) bool {
	return form != formUntagged && !fs.hasFlattenDe()
}

// visitorForm returns the struct visitor of form, without visit_seq where
// the form has none.
func (fs *fieldSet) visitorForm(form structForm, variantIdent string, base unsafe.Pointer) DeVisitor {
	visitor := fs.structVisitorFor(form, variantIdent, base)
	if !fs.hasVisitSeq(form) {
		return &visitor.fieldsVisitor
	}
	return visitor
}

// deserializeStruct is `struct_::deserialize` for a struct (form Struct).
func (fs *fieldSet) deserializeStruct(deserializer Deserializer, base unsafe.Pointer) error {
	visitor := fs.visitorForm(formStruct, "", base)
	if fs.hasFlattenDe() {
		_, err := deserializer.DeserializeMap(visitor)
		return err
	}
	_, err := deserializer.DeserializeStruct(fs.cont.name.deserialize, fs.fieldNames, visitor)
	return err
}

// tupleExpecting is the expecting text of `tuple::deserialize`.
func (fs *fieldSet) tupleExpecting(variantIdent string) string {
	expecting := "tuple struct " + fs.typeName
	if variantIdent != "" {
		expecting = "tuple variant " + fs.typeName + "::" + variantIdent
	}
	if fs.cont.hasExpecting {
		expecting = fs.cont.expecting
	}
	return expecting
}

func (fs *fieldSet) deserializedCount() int {
	count := 0
	for _, f := range fs.fields {
		if !f.attrs.skipDeserializing {
			count++
		}
	}
	return count
}

// deserializeTupleStruct is `tuple::deserialize` with TupleForm::Tuple.
func (fs *fieldSet) deserializeTupleStruct(deserializer Deserializer, base unsafe.Pointer) error {
	expecting := fs.tupleExpecting("")
	if len(fs.fields) == 1 {
		visitor := &newtypeVisitor{tupleFieldsVisitor{fs: fs, base: base, expecting: expecting}}
		_, err := deserializer.DeserializeNewtypeStruct(fs.cont.name.deserialize, visitor)
		return err
	}
	visitor := &tupleFieldsVisitor{fs: fs, base: base, expecting: expecting}
	_, err := deserializer.DeserializeTupleStruct(fs.cont.name.deserialize, fs.deserializedCount(), visitor)
	return err
}

// unitStructVisitor is the `__Visitor` of `unit::deserialize`.
type unitStructVisitor struct{ expecting string }

func (v *unitStructVisitor) Expecting() string       { return v.expecting }
func (v *unitStructVisitor) VisitUnit() (any, error) { return nil, nil }

// deserialize is `deserialize_body` for a struct.
func (d *derivedStruct) deserialize(p unsafe.Pointer, deserializer Deserializer) error {
	fs := d.fields
	switch {
	case fs.cont.transparent:
		return deserializeTransparent(fs, p, deserializer)
	case d.from != nil:
		return d.from(p, deserializer)
	}
	switch fs.style {
	case styleStruct:
		return fs.deserializeStruct(deserializer, p)
	case styleTuple, styleNewtype:
		return fs.deserializeTupleStruct(deserializer, p)
	case styleUnit:
		expecting := "unit struct " + fs.typeName
		if fs.cont.hasExpecting {
			expecting = fs.cont.expecting
		}
		_, err := deserializer.DeserializeUnitStruct(fs.cont.name.deserialize, &unitStructVisitor{expecting: expecting})
		return err
	}
	panic(rs.Unreachable(fs.style))
}

// deserializeTransparent is `deserialize_transparent`.
func deserializeTransparent(fs *fieldSet, p unsafe.Pointer, deserializer Deserializer) error {
	var transparent *field
	for _, f := range fs.fields {
		if f.attrs.transparentDe {
			transparent = f
		}
	}
	if transparent == nil {
		panic(rs.Panicf("serde: " + fs.where + ": #[serde(transparent)] requires at least one field that is neither skipped nor has a default"))
	}
	if err := transparent.deserialize(p, deserializer); err != nil {
		return err
	}
	for _, f := range fs.fields {
		if f == transparent {
			continue
		}
		switch f.attrs.dflt.kind {
		case defaultDefault, defaultPath:
			f.setDefault(f.pointer(p))
		default:
			// PhantomData
			reflect.NewAt(f.typ, f.pointer(p)).Elem().SetZero()
		}
	}
	return nil
}

// variantIdentifier is `prepare_enum_variant_enum`: the identifier of the
// variants that are deserialized, and `VARIANTS`.
func (e *derivedEnum) variantIdentifier() *identifier {
	id := &identifier{isVariant: true, expecting: "variant identifier"}
	for _, v := range e.deVariants {
		id.aliases = append(id.aliases, v.attrs.name.aliases)
		id.names = append(id.names, v.attrs.name.aliases...)
	}
	for index, v := range e.deVariants {
		if v.attrs.other {
			id.fallback, id.otherIndex = fallthroughOther, index
			break
		}
	}
	return id
}

// newValue returns fresh storage for a value of variant v, and the function
// that stores it as the enum value at p.
func (e *derivedEnum) newValue(v *variant) (unsafe.Pointer, func(p unsafe.Pointer)) {
	if v.goType == nil {
		return nil, func(p unsafe.Pointer) { e.storeConst(p, v.value) }
	}
	fresh := reflect.New(v.goType)
	return fresh.UnsafePointer(), func(p unsafe.Pointer) {
		reflect.NewAt(e.typ, p).Elem().Set(fresh)
	}
}

// storeConst stores the data-free enum value bits at p.
func (e *derivedEnum) storeConst(p unsafe.Pointer, bits uint64) {
	value := reflect.NewAt(e.typ, p).Elem()
	switch value.Kind() {
	case reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64:
		value.SetInt(int64(bits))
	default:
		value.SetUint(bits)
	}
}

// deserialize is `enum_::deserialize`.
func (e *derivedEnum) deserialize(p unsafe.Pointer, deserializer Deserializer) error {
	if e.from != nil {
		return e.from(p, deserializer)
	}
	if e.cont.identifier != identifierNo {
		return e.deserializeCustomIdentifier(p, deserializer)
	}
	if e.firstUntagged >= 0 {
		return e.deserializeUntagged(p, deserializer, true)
	}
	return e.deserializeHomogeneous(p, deserializer)
}

// deserializeHomogeneous is `deserialize_homogeneous_enum`.
func (e *derivedEnum) deserializeHomogeneous(p unsafe.Pointer, deserializer Deserializer) error {
	switch e.cont.tag {
	case tagExternal:
		return e.deserializeExternallyTagged(p, deserializer)
	case tagInternal:
		return e.deserializeInternallyTagged(p, deserializer)
	case tagAdjacent:
		return e.deserializeAdjacentlyTagged(p, deserializer)
	}
	return e.deserializeUntagged(p, deserializer, false)
}

// externalVisitor is the `__Visitor` of an externally tagged enum.
type externalVisitor struct {
	e         *derivedEnum
	p         unsafe.Pointer
	expecting string
}

func (v *externalVisitor) Expecting() string { return v.expecting }

func (v *externalVisitor) VisitEnum(data DeEnumAccess) (any, error) {
	key, access, err := data.VariantSeed(v.e.variantIdentifier())
	if err != nil {
		return nil, err
	}
	variant := v.e.deVariants[key.(identKey).index]
	return nil, v.e.deserializeExternallyTaggedVariant(variant, access, v.p)
}

// deserializeExternallyTagged is `enum_externally::deserialize`.
func (e *derivedEnum) deserializeExternallyTagged(p unsafe.Pointer, deserializer Deserializer) error {
	expecting := "enum " + e.typeName
	if e.cont.hasExpecting {
		expecting = e.cont.expecting
	}
	_, err := deserializer.DeserializeEnum(e.cont.name.deserialize, e.variantNames, &externalVisitor{e: e, p: p, expecting: expecting})
	return err
}

// variantWithSeed is the `__DeserializeWith` of a variant's
// deserialize_with function.
type variantWithSeed struct {
	v    *variant
	base unsafe.Pointer
}

func (s *variantWithSeed) DeserializeSeed(deserializer Deserializer) (any, error) {
	return nil, s.v.deWith(s.base, deserializer)
}

// structVariantSeed is the `DeserializeSeed` impl of `__Visitor` that an
// externally tagged struct variant with a flattened field reads its map
// with.
type structVariantSeed struct{ visitor DeVisitor }

func (s *structVariantSeed) DeserializeSeed(deserializer Deserializer) (any, error) {
	return deserializer.DeserializeMap(s.visitor)
}

// deserializeExternallyTaggedVariant is
// `deserialize_externally_tagged_variant`.
func (e *derivedEnum) deserializeExternallyTaggedVariant(v *variant, access DeVariantAccess, p unsafe.Pointer) error {
	base, store := e.newValue(v)
	if v.deWith != nil {
		if _, err := access.NewtypeVariantSeed(&variantWithSeed{v: v, base: base}); err != nil {
			return err
		}
		store(p)
		return nil
	}
	switch v.style {
	case styleUnit:
		if err := access.UnitVariant(); err != nil {
			return err
		}
	case styleNewtype:
		f := v.fields.fields[0]
		if f.attrs.skipDeserializing {
			if err := access.UnitVariant(); err != nil {
				return err
			}
			if err := v.fields.missing(f, base, nil); err != nil {
				return err
			}
			break
		}
		if _, err := access.NewtypeVariantSeed(f.seed(base)); err != nil {
			return err
		}
	case styleTuple:
		visitor := &tupleFieldsVisitor{fs: v.fields, base: base, expecting: v.fields.tupleExpecting(v.ident)}
		if _, err := access.TupleVariant(v.fields.deserializedCount(), visitor); err != nil {
			return err
		}
	case styleStruct:
		visitor := v.fields.visitorForm(formExternallyTagged, v.ident, base)
		if v.fields.hasFlattenDe() {
			if _, err := access.NewtypeVariantSeed(&structVariantSeed{visitor: visitor}); err != nil {
				return err
			}
		} else if _, err := access.StructVariant(v.fields.fieldNames, visitor); err != nil {
			return err
		}
	}
	store(p)
	return nil
}

// deserializeInternallyTagged is `enum_internally::deserialize`.
func (e *derivedEnum) deserializeInternallyTagged(p unsafe.Pointer, deserializer Deserializer) error {
	expecting := "internally tagged enum " + e.typeName
	if e.cont.hasExpecting {
		expecting = e.cont.expecting
	}
	tag := &capturingSeed{seed: e.variantIdentifier()}
	var buffered content
	visitor := &taggedContentVisitor{tagName: e.cont.tagName, expecting: expecting, tagSeed: tag, out: &buffered}
	if _, err := deserializer.DeserializeAny(visitor); err != nil {
		return err
	}
	v := e.deVariants[tag.value.(identKey).index]
	return e.deserializeInternallyTaggedVariant(v, newContentDeserializer(&buffered), p)
}

// capturingSeed keeps the value its seed produced.
type capturingSeed struct {
	seed  DeDeserializeSeed
	value any
}

func (s *capturingSeed) DeserializeSeed(deserializer Deserializer) (any, error) {
	value, err := s.seed.DeserializeSeed(deserializer)
	if err == nil {
		s.value = value
	}
	return value, err
}

// deserializeInternallyTaggedVariant is
// `deserialize_internally_tagged_variant`.
func (e *derivedEnum) deserializeInternallyTaggedVariant(v *variant, deserializer Deserializer, p unsafe.Pointer) error {
	base, store := e.newValue(v)
	if v.deWith != nil {
		if err := v.deWith(base, deserializer); err != nil {
			return err
		}
		store(p)
		return nil
	}
	switch v.effectiveStyleDe() {
	case styleUnit:
		if _, err := deserializer.DeserializeAny(&internallyTaggedUnitVisitor{typeName: e.typeName, variantName: v.ident}); err != nil {
			return err
		}
		if len(v.fields.fields) > 0 {
			if err := v.fields.missing(v.fields.fields[0], base, nil); err != nil {
				return err
			}
		}
	case styleNewtype:
		if err := v.fields.fields[0].deserialize(base, deserializer); err != nil {
			return err
		}
	case styleStruct:
		visitor := v.fields.visitorForm(formInternallyTagged, v.ident, base)
		if _, err := deserializer.DeserializeAny(visitor); err != nil {
			return err
		}
	default:
		panic(rs.Unreachable("serde_derive: internally tagged tuple variant, checked in serde_derive_internals"))
	}
	store(p)
	return nil
}

// deserializeUntaggedVariant is `enum_untagged::deserialize_variant`, which
// adjacently tagged enums also use.
func (e *derivedEnum) deserializeUntaggedVariant(v *variant, deserializer Deserializer, p unsafe.Pointer) error {
	base, store := e.newValue(v)
	if v.deWith != nil {
		if err := v.deWith(base, deserializer); err != nil {
			return err
		}
		store(p)
		return nil
	}
	switch v.effectiveStyleDe() {
	case styleUnit:
		if _, err := deserializer.DeserializeAny(&untaggedUnitVisitor{typeName: e.typeName, variantName: v.ident}); err != nil {
			return err
		}
		if len(v.fields.fields) > 0 {
			if err := v.fields.missing(v.fields.fields[0], base, nil); err != nil {
				return err
			}
		}
	case styleNewtype:
		if err := v.fields.fields[0].deserialize(base, deserializer); err != nil {
			return err
		}
	case styleTuple:
		visitor := &tupleFieldsVisitor{fs: v.fields, base: base, expecting: v.fields.tupleExpecting(v.ident)}
		if _, err := deserializer.DeserializeTuple(v.fields.deserializedCount(), visitor); err != nil {
			return err
		}
	case styleStruct:
		visitor := v.fields.visitorForm(formUntagged, v.ident, base)
		if _, err := deserializer.DeserializeAny(visitor); err != nil {
			return err
		}
	}
	store(p)
	return nil
}

// deserializeUntagged is `enum_untagged::deserialize`. mixed says the enum
// has tagged variants before its untagged ones, which are tried first.
func (e *derivedEnum) deserializeUntagged(p unsafe.Pointer, deserializer Deserializer, mixed bool) error {
	var buffered content
	if _, err := (contentVisitor{out: &buffered}).DeserializeSeed(deserializer); err != nil {
		return err
	}
	ref := newContentRefDeserializer(&buffered)
	variants := e.deVariants
	if mixed {
		// Ignore any error associated with non-untagged deserialization so
		// that we can fall through to the untagged variants.
		if e.tagged.deserializeHomogeneous(p, ref) == nil {
			return nil
		}
		variants = e.untaggedVariants
	}
	for _, v := range variants {
		if e.deserializeUntaggedVariant(v, ref, p) == nil {
			return nil
		}
	}
	message := "data did not match any variant of untagged enum " + e.typeName
	if e.cont.hasExpecting {
		message = e.cont.expecting
	}
	return DeErrorCustom(message)
}

// adjacentVisitor is the `__Visitor` of an adjacently tagged enum.
type adjacentVisitor struct {
	e         *derivedEnum
	p         unsafe.Pointer
	expecting string
}

func (v *adjacentVisitor) Expecting() string { return v.expecting }

// nextRelevantKey is `next_relevant_key`: the next key that is the tag or
// the content, stepping over others unless unknown fields are denied.
func (v *adjacentVisitor) nextRelevantKey(m DeMapAccess) (tagOrContentField, bool, error) {
	cont := v.e.cont
	if cont.denyUnknownFields {
		key, ok, err := m.NextKeySeed(&tagOrContentFieldVisitor{tag: cont.tagName, content: cont.contentName})
		if err != nil || !ok {
			return 0, false, err
		}
		return key.(tagOrContentField), true, nil
	}
	for {
		key, ok, err := m.NextKeySeed(&tagContentOtherFieldVisitor{tag: cont.tagName, content: cont.contentName})
		if err != nil || !ok {
			return 0, false, err
		}
		switch key.(tagContentOtherField) {
		case tagContentOtherFieldOther:
			if _, err := m.NextValueSeed(DeIgnoredAny{}); err != nil {
				return 0, false, err
			}
			continue
		case tagContentOtherFieldTag:
			return tagOrContentFieldTag, true, nil
		}
		return tagOrContentFieldContent, true, nil
	}
}

// visitRemainingKeys is `visit_remaining_keys`.
func (v *adjacentVisitor) visitRemainingKeys(m DeMapAccess) error {
	key, ok, err := v.nextRelevantKey(m)
	if err != nil {
		return err
	}
	if !ok {
		return nil
	}
	if key == tagOrContentFieldTag {
		return DeErrorDuplicateField(v.e.cont.tagName)
	}
	return DeErrorDuplicateField(v.e.cont.contentName)
}

// variantFromMap is `variant_from_map`: the tag's value, read as a unit
// variant of an enum named for the Rust type.
func (v *adjacentVisitor) variantFromMap(m DeMapAccess) (*variant, error) {
	seed := &capturingSeed{seed: v.e.variantIdentifier()}
	if _, err := m.NextValueSeed(&adjacentlyTaggedEnumVariantSeed{enumName: v.e.typeName, variants: v.e.variantNames, fieldSeed: seed}); err != nil {
		return nil, err
	}
	return v.e.deVariants[seed.value.(identKey).index], nil
}

// adjacentSeed is `__Seed`: the content of the variant the tag named.
type adjacentSeed struct {
	e *derivedEnum
	v *variant
	p unsafe.Pointer
}

func (s *adjacentSeed) DeserializeSeed(deserializer Deserializer) (any, error) {
	return nil, s.e.deserializeUntaggedVariant(s.v, deserializer, s.p)
}

// missingContent is `missing_content`: the value when the tag came without
// content.
func (v *adjacentVisitor) missingContent(variant *variant) error {
	switch {
	case variant.style == styleUnit:
		_, store := v.e.newValue(variant)
		store(v.p)
		return nil
	case variant.style == styleNewtype && variant.deWith == nil:
		base, store := v.e.newValue(variant)
		f := variant.fields.fields[0]
		if err := f.codec().de(f.pointer(base), missingFieldDeserializer{field: v.e.cont.contentName}); err != nil {
			return err
		}
		store(v.p)
		return nil
	}
	return DeErrorMissingField(v.e.cont.contentName)
}

func (v *adjacentVisitor) VisitMap(m DeMapAccess) (any, error) {
	cont := v.e.cont
	// Visit the first relevant key.
	first, ok, err := v.nextRelevantKey(m)
	if err != nil {
		return nil, err
	}
	if !ok {
		// There is no first key.
		return nil, DeErrorMissingField(cont.tagName)
	}
	if first == tagOrContentFieldTag {
		// First key is the tag. Parse the tag.
		variant, err := v.variantFromMap(m)
		if err != nil {
			return nil, err
		}
		// Visit the second key.
		second, ok, err := v.nextRelevantKey(m)
		if err != nil {
			return nil, err
		}
		if !ok {
			// There is no second key; might be okay if the we have a unit
			// variant.
			return nil, v.missingContent(variant)
		}
		if second == tagOrContentFieldTag {
			// Second key is a duplicate of the tag.
			return nil, DeErrorDuplicateField(cont.tagName)
		}
		// Second key is the content.
		if _, err := m.NextValueSeed(&adjacentSeed{e: v.e, v: variant, p: v.p}); err != nil {
			return nil, err
		}
		// Visit remaining keys, looking for duplicates.
		return nil, v.visitRemainingKeys(m)
	}
	// First key is the content. Buffer up the content.
	var buffered content
	if _, err := m.NextValueSeed(contentVisitor{out: &buffered}); err != nil {
		return nil, err
	}
	// Visit the second key.
	second, ok, err := v.nextRelevantKey(m)
	if err != nil {
		return nil, err
	}
	if !ok {
		// There is no second key.
		return nil, DeErrorMissingField(cont.tagName)
	}
	if second == tagOrContentFieldContent {
		// Second key is a duplicate of the content.
		return nil, DeErrorDuplicateField(cont.contentName)
	}
	// Second key is the tag.
	variant, err := v.variantFromMap(m)
	if err != nil {
		return nil, err
	}
	if err := v.e.deserializeUntaggedVariant(variant, newContentDeserializer(&buffered), v.p); err != nil {
		return nil, err
	}
	// Visit remaining keys, looking for duplicates.
	return nil, v.visitRemainingKeys(m)
}

func (v *adjacentVisitor) VisitSeq(seq DeSeqAccess) (any, error) {
	// Visit the first element - the tag.
	tag := &capturingSeed{seed: v.e.variantIdentifier()}
	_, ok, err := seq.NextElementSeed(tag)
	if err != nil {
		return nil, err
	}
	if !ok {
		// There is no first element.
		return nil, DeErrorInvalidLength(0, v)
	}
	variant := v.e.deVariants[tag.value.(identKey).index]
	// Visit the second element - the content.
	_, ok, err = seq.NextElementSeed(&adjacentSeed{e: v.e, v: variant, p: v.p})
	if err != nil {
		return nil, err
	}
	if !ok {
		// There is no second element.
		return nil, DeErrorInvalidLength(1, v)
	}
	return nil, nil
}

// deserializeAdjacentlyTagged is `enum_adjacently::deserialize`.
func (e *derivedEnum) deserializeAdjacentlyTagged(p unsafe.Pointer, deserializer Deserializer) error {
	expecting := "adjacently tagged enum " + e.typeName
	if e.cont.hasExpecting {
		expecting = e.cont.expecting
	}
	fields := []string{e.cont.tagName, e.cont.contentName}
	_, err := deserializer.DeserializeStruct(e.cont.name.deserialize, fields, &adjacentVisitor{e: e, p: p, expecting: expecting})
	return err
}

// customIdentifierVisitor is the `__FieldVisitor` of an enum with
// `#[serde(field_identifier)]` or `#[serde(variant_identifier)]`.
type customIdentifierVisitor struct {
	e  *derivedEnum
	id *identifier
	p  unsafe.Pointer
}

func (v *customIdentifierVisitor) Expecting() string { return v.id.expecting }

func (v *customIdentifierVisitor) store(key any, err error) (any, error) {
	if err != nil {
		return nil, err
	}
	_, store := v.e.newValue(v.e.ordinary[key.(identKey).index])
	store(v.p)
	return nil, nil
}

func (v *customIdentifierVisitor) VisitU64(value uint64) (any, error) {
	return v.store((&identVisitor{id: v.id}).VisitU64(value))
}

func (v *customIdentifierVisitor) VisitStr(value string) (any, error) {
	return v.store((&identVisitor{id: v.id}).VisitStr(value))
}

func (v *customIdentifierVisitor) VisitBytes(value []byte) (any, error) {
	return v.store((&identVisitor{id: v.id}).VisitBytes(value))
}

// deserializeCustomIdentifier is `identifier::deserialize_custom`.
func (e *derivedEnum) deserializeCustomIdentifier(p unsafe.Pointer, deserializer Deserializer) error {
	isVariant := e.cont.identifier == identifierVariant
	id := &identifier{isVariant: isVariant}
	ordinary := e.variants
	if last := e.variants[len(e.variants)-1]; last.attrs.other {
		ordinary = e.variants[:len(e.variants)-1]
		id.fallback, id.otherIndex = fallthroughOther, len(ordinary)
	} else if last.style == styleNewtype {
		panic(rs.Todo("serde_derive::de::identifier: a field or variant identifier whose last variant is a newtype"))
	}
	e.ordinary = e.variants
	for _, v := range ordinary {
		id.aliases = append(id.aliases, v.attrs.name.aliases)
		if id.fallback != fallthroughOther {
			id.names = append(id.names, v.attrs.name.aliases...)
		}
	}
	switch {
	case e.cont.hasExpecting:
		id.expecting = e.cont.expecting
	case isVariant:
		id.expecting = "variant identifier"
	default:
		id.expecting = "field identifier"
	}
	_, err := deserializer.DeserializeIdentifier(&customIdentifierVisitor{e: e, id: id, p: p})
	return err
}
