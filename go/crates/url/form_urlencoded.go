// Port of form_urlencoded 1.2.2 src/lib.rs

package url

import (
	"bytes"
	"iter"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/strs"
)

// Parser and serializer for the `application/x-www-form-urlencoded` syntax
// (http://url.spec.whatwg.org/#application/x-www-form-urlencoded), as used by
// HTML forms.
//
// Converts between a string (such as an URL’s query string) and a sequence of
// (name, value) pairs.
//
// Rust code reaches this crate as `url::form_urlencoded`, so its items carry
// the prefix FormUrlencoded. The functions `parse` and `byte_serialize` and
// the iterator types `Parse` and `ByteSerialize` they return would get the
// same Go name; the functions keep it, and the types are
// FormUrlencodedParseIter and FormUrlencodedByteSerializeIter.

// FormUrlencodedParse is form_urlencoded::parse: convert a byte string in the
// `application/x-www-form-urlencoded` syntax into a iterator of (name, value)
// pairs.
//
// Use `FormUrlencodedParse([]byte(inputStr))` to parse a string.
//
// The names and values are percent-decoded. For instance,
// `%23first=%25try%25` will be converted to `[("#first", "%try%")]`.
func FormUrlencodedParse(input []byte) FormUrlencodedParseIter {
	return FormUrlencodedParseIter{input: input}
}

// FormUrlencodedParseIter is form_urlencoded::Parse: the return type of
// FormUrlencodedParse. It is a Copy type in Rust: a copy of the value
// iterates on its own.
type FormUrlencodedParseIter struct {
	input []byte
}

// Next is Iterator::next: the next (name, value) pair, percent-decoded and
// with invalid UTF-8 replaced as String::from_utf8_lossy replaces it.
func (p *FormUrlencodedParseIter) Next() rs.Option[rs.Tuple2[string, string]] {
	for {
		if len(p.input) == 0 {
			return rs.None[rs.Tuple2[string, string]]()
		}
		var sequence []byte
		if position := bytes.IndexByte(p.input, '&'); position >= 0 {
			sequence = p.input[:position]
			p.input = p.input[position+1:]
		} else {
			sequence = p.input
			p.input = nil
		}
		if len(sequence) == 0 {
			continue
		}
		name := sequence
		var value []byte
		if position := bytes.IndexByte(sequence, '='); position >= 0 {
			name = sequence[:position]
			value = sequence[position+1:]
		}
		return rs.Some(rs.T2(formUrlencodedDecode(name), formUrlencodedDecode(value)))
	}
}

func formUrlencodedDecode(input []byte) string {
	replaced := formUrlencodedReplacePlus(input)
	decoded, _ := percentEncodingPercentDecodeNew(replaced).collect()
	return formUrlencodedDecodeUtf8Lossy(decoded)
}

// Replace b'+' with b' '
func formUrlencodedReplacePlus(input []byte) []byte {
	firstPosition := bytes.IndexByte(input, '+')
	if firstPosition < 0 {
		return input
	}
	replaced := bytes.Clone(input)
	replaced[firstPosition] = ' '
	for i := firstPosition + 1; i < len(replaced); i++ {
		if replaced[i] == '+' {
			replaced[i] = ' '
		}
	}
	return replaced
}

// IntoOwned is Parse::into_owned: return a new iterator that yields pairs of
// `String` instead of pairs of `Cow<str>`. In Go both yield strings.
func (p FormUrlencodedParseIter) IntoOwned() FormUrlencodedParseIntoOwned {
	return FormUrlencodedParseIntoOwned{inner: p}
}

// Count is Iterator::count on a copy of the iterator: the number of pairs
// left.
func (p FormUrlencodedParseIter) Count() int {
	count := 0
	for p.Next().IsSome() {
		count++
	}
	return count
}

// Collect is `.collect::<Vec<_>>()`: the pairs left, in order.
func (p FormUrlencodedParseIter) Collect() []rs.Tuple2[string, string] {
	var pairs []rs.Tuple2[string, string]
	for {
		pair, ok := p.Next().Get()
		if !ok {
			return pairs
		}
		pairs = append(pairs, pair)
	}
}

// All ranges over the pairs left, for `for (name, value) in parse(..)`:
//
//	for name, value := range url.FormUrlencodedParse(query).All() { ... }
func (p FormUrlencodedParseIter) All() iter.Seq2[string, string] {
	return func(yield func(string, string) bool) {
		for {
			pair, ok := p.Next().Get()
			if !ok || !yield(pair.F0, pair.F1) {
				return
			}
		}
	}
}

// FormUrlencodedParseIntoOwned is form_urlencoded::ParseIntoOwned: like
// FormUrlencodedParseIter, but yields pairs of `String` instead of pairs of
// `Cow<str>`.
type FormUrlencodedParseIntoOwned struct {
	inner FormUrlencodedParseIter
}

// Next is Iterator::next.
func (p *FormUrlencodedParseIntoOwned) Next() rs.Option[rs.Tuple2[string, string]] {
	return p.inner.Next()
}

// Collect is `.collect::<Vec<_>>()`: the pairs left, in order.
func (p FormUrlencodedParseIntoOwned) Collect() []rs.Tuple2[string, string] {
	return p.inner.Collect()
}

// All ranges over the pairs left.
func (p FormUrlencodedParseIntoOwned) All() iter.Seq2[string, string] {
	return p.inner.All()
}

// FormUrlencodedByteSerialize is form_urlencoded::byte_serialize: the
// application/x-www-form-urlencoded byte serializer
// (https://url.spec.whatwg.org/#concept-urlencoded-byte-serializer).
//
// Return an iterator of `&str` slices.
func FormUrlencodedByteSerialize(input []byte) FormUrlencodedByteSerializeIter {
	return FormUrlencodedByteSerializeIter{bytes: input}
}

// FormUrlencodedByteSerializeIter is form_urlencoded::ByteSerialize: the
// return type of FormUrlencodedByteSerialize.
type FormUrlencodedByteSerializeIter struct {
	bytes []byte
}

func formUrlencodedByteSerializedUnchanged(b byte) bool {
	return b == '*' || b == '-' || b == '.' || ('0' <= b && b <= '9') || ('A' <= b && b <= 'Z') || b == '_' || ('a' <= b && b <= 'z')
}

// Next is Iterator::next: the next slice of the serialization.
func (s *FormUrlencodedByteSerializeIter) Next() rs.Option[string] {
	if len(s.bytes) == 0 {
		return rs.None[string]()
	}
	first := s.bytes[0]
	tail := s.bytes[1:]
	if !formUrlencodedByteSerializedUnchanged(first) {
		s.bytes = tail
		if first == ' ' {
			return rs.Some("+")
		}
		return rs.Some(percentEncodingPercentEncodeByte(first))
	}
	position := -1
	for i, b := range tail {
		if !formUrlencodedByteSerializedUnchanged(b) {
			position = i
			break
		}
	}
	var unchangedSlice, remaining []byte
	if position >= 0 {
		// 1 for first_byte + i unchanged in tail
		unchangedSlice, remaining = s.bytes[:1+position], s.bytes[1+position:]
	} else {
		unchangedSlice, remaining = s.bytes, nil
	}
	s.bytes = remaining
	return rs.Some(string(unchangedSlice))
}

// Collect is `.collect::<String>()`: the whole serialization.
func (s FormUrlencodedByteSerializeIter) Collect() string {
	return string(formUrlencodedAppendByteSerialize(nil, s.bytes))
}

func formUrlencodedAppendByteSerialize(out []byte, input []byte) []byte {
	for _, b := range input {
		switch {
		case formUrlencodedByteSerializedUnchanged(b):
			out = append(out, b)
		case b == ' ':
			out = append(out, '+')
		default:
			out = append(out, percentEncodingPercentEncodeByte(b)...)
		}
	}
	return out
}

// FormUrlencodedSerializer is form_urlencoded::Serializer: the
// application/x-www-form-urlencoded serializer
// (https://url.spec.whatwg.org/#concept-urlencoded-serializer).
//
// The type parameter is the target's Finished type. A serializer that owns a
// target which must be dropped (the one Url.QueryPairsMut returns) is dropped
// with Drop, or ended with Finish.
type FormUrlencodedSerializer[F any] struct {
	target        rs.Option[FormUrlencodedTarget[F]]
	startPosition int
	encoding      FormUrlencodedEncodingOverride
}

// FormUrlencodedTarget is form_urlencoded::Target, with its associated type
// Finished as the type parameter.
type FormUrlencodedTarget[F any] interface {
	AsMutString() *string
	Finish() F
}

type formUrlencodedStringTarget struct {
	s string
}

func (t *formUrlencodedStringTarget) AsMutString() *string { return &t.s }

func (t *formUrlencodedStringTarget) Finish() string { return t.s }

// FormUrlencodedTargetString is `impl Target for String`: a target that owns
// the string, and whose Finish returns it.
func FormUrlencodedTargetString(s string) FormUrlencodedTarget[string] {
	return &formUrlencodedStringTarget{s: s}
}

type formUrlencodedMutStringTarget struct {
	s *string
}

func (t *formUrlencodedMutStringTarget) AsMutString() *string { return t.s }

func (t *formUrlencodedMutStringTarget) Finish() *string { return t.s }

// FormUrlencodedTargetMutString is `impl Target for &mut String`: a target
// that writes to the caller's string, and whose Finish returns the pointer.
func FormUrlencodedTargetMutString(s *string) FormUrlencodedTarget[*string] {
	return &formUrlencodedMutStringTarget{s: s}
}

// FormUrlencodedSerializerNew is Serializer::new: create a new
// `application/x-www-form-urlencoded` serializer for the given target.
//
// If the target is non-empty, its content is assumed to already be in
// `application/x-www-form-urlencoded` syntax.
func FormUrlencodedSerializerNew[F any](target FormUrlencodedTarget[F]) *FormUrlencodedSerializer[F] {
	return FormUrlencodedSerializerForSuffix(target, 0)
}

// FormUrlencodedSerializerForSuffix is Serializer::for_suffix: create a new
// `application/x-www-form-urlencoded` serializer for a suffix of the given
// target.
//
// If that suffix is non-empty, its content is assumed to already be in
// `application/x-www-form-urlencoded` syntax.
func FormUrlencodedSerializerForSuffix[F any](target FormUrlencodedTarget[F], startPosition int) *FormUrlencodedSerializer[F] {
	if len(*target.AsMutString()) < startPosition {
		rs.PanicAt(1, "invalid length "+itoa(startPosition)+" for target of length "+itoa(len(*target.AsMutString())))
	}

	return &FormUrlencodedSerializer[F]{
		target:        rs.Some(target),
		startPosition: startPosition,
		encoding:      rs.None[func(string) []byte](),
	}
}

// Clear is Serializer::clear: remove any existing name/value pair.
//
// Panics if called after `.Finish()`.
func (s *FormUrlencodedSerializer[F]) Clear() *FormUrlencodedSerializer[F] {
	str := formUrlencodedString(&s.target)
	*str = (*str)[:s.startPosition]
	return s
}

// EncodingOverride is Serializer::encoding_override: set the character
// encoding to be used for names and values before percent-encoding.
func (s *FormUrlencodedSerializer[F]) EncodingOverride(new FormUrlencodedEncodingOverride) *FormUrlencodedSerializer[F] {
	s.encoding = new
	return s
}

// AppendPair is Serializer::append_pair: serialize and append a name/value
// pair.
//
// Panics if called after `.Finish()`.
func (s *FormUrlencodedSerializer[F]) AppendPair(name string, value string) *FormUrlencodedSerializer[F] {
	formUrlencodedAppendPair(
		formUrlencodedString(&s.target),
		s.startPosition,
		s.encoding,
		name,
		value,
	)
	return s
}

// AppendKeyOnly is Serializer::append_key_only: serialize and append a name
// of parameter without any value.
//
// Panics if called after `.Finish()`.
func (s *FormUrlencodedSerializer[F]) AppendKeyOnly(name string) *FormUrlencodedSerializer[F] {
	formUrlencodedAppendKeyOnly(
		formUrlencodedString(&s.target),
		s.startPosition,
		s.encoding,
		name,
	)
	return s
}

// ExtendPairs is Serializer::extend_pairs: serialize and append a number of
// name/value pairs.
//
// This simply calls `AppendPair` repeatedly. This can be more efficient, so
// the user may choose to.
//
// Panics if called after `.Finish()`.
func (s *FormUrlencodedSerializer[F]) ExtendPairs(pairs []rs.Tuple2[string, string]) *FormUrlencodedSerializer[F] {
	str := formUrlencodedString(&s.target)
	for _, pair := range pairs {
		formUrlencodedAppendPair(
			str,
			s.startPosition,
			s.encoding,
			pair.F0,
			pair.F1,
		)
	}
	return s
}

// ExtendKeysOnly is Serializer::extend_keys_only: serialize and append a
// number of names without values.
//
// This simply calls `AppendKeyOnly` repeatedly. This can be more efficient,
// so the user may choose to.
//
// Panics if called after `.Finish()`.
func (s *FormUrlencodedSerializer[F]) ExtendKeysOnly(keys []string) *FormUrlencodedSerializer[F] {
	str := formUrlencodedString(&s.target)
	for _, key := range keys {
		formUrlencodedAppendKeyOnly(str, s.startPosition, s.encoding, key)
	}
	return s
}

// Finish is Serializer::finish: if this serializer was constructed with a
// string, take and return that string.
//
// Panics if called more than once.
func (s *FormUrlencodedSerializer[F]) Finish() F {
	target, ok := s.target.Take().Get()
	if !ok {
		rs.PanicAt(1, "url::form_urlencoded::Serializer double finish")
	}
	return target.Finish()
}

// Drop drops the serializer, and with it a target that has not been
// finished. It is what the end of a statement such as
// `url.query_pairs_mut().append_pair("a", "b");` does in Rust: the target
// that Url.QueryPairsMut made puts the URL's fragment back.
func (s *FormUrlencodedSerializer[F]) Drop() {
	target, ok := s.target.Take().Get()
	if !ok {
		return
	}
	if droppable, ok := target.(interface{ Drop() }); ok {
		droppable.Drop()
	}
}

func formUrlencodedAppendSeparatorIfNeeded(str *string, startPosition int) {
	if len(*str) > startPosition {
		*str += "&"
	}
}

func formUrlencodedString[F any](target *rs.Option[FormUrlencodedTarget[F]]) *string {
	value, ok := target.Get()
	if !ok {
		rs.PanicAt(2, "url::form_urlencoded::Serializer finished")
	}
	return value.AsMutString()
}

func formUrlencodedAppendPair(
	str *string,
	startPosition int,
	encoding FormUrlencodedEncodingOverride,
	name string,
	value string,
) {
	formUrlencodedAppendSeparatorIfNeeded(str, startPosition)
	formUrlencodedAppendEncoded(name, str, encoding)
	*str += "="
	formUrlencodedAppendEncoded(value, str, encoding)
}

func formUrlencodedAppendKeyOnly(
	str *string,
	startPosition int,
	encoding FormUrlencodedEncodingOverride,
	name string,
) {
	formUrlencodedAppendSeparatorIfNeeded(str, startPosition)
	formUrlencodedAppendEncoded(name, str, encoding)
}

func formUrlencodedAppendEncoded(s string, str *string, encoding FormUrlencodedEncodingOverride) {
	*str += FormUrlencodedByteSerialize(formUrlencodedEncode(encoding, s)).Collect()
}

func formUrlencodedEncode(encodingOverride FormUrlencodedEncodingOverride, input string) []byte {
	if o, ok := encodingOverride.Get(); ok {
		return o(input)
	}
	return []byte(input)
}

func formUrlencodedDecodeUtf8Lossy(input []byte) string {
	// Note: This function is duplicated in `percent_encoding/lib.rs`.
	return strs.FromUtf8Lossy(input)
}

// FormUrlencodedEncodingOverride is form_urlencoded::EncodingOverride, which
// url re-exports as url::EncodingOverride: a function that encodes a string
// in another character encoding, or None for UTF-8.
type FormUrlencodedEncodingOverride = rs.Option[func(string) []byte]
