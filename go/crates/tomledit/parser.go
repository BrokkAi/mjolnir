// Port of toml_edit 0.20.2 src/parser/mod.rs

package tomledit

import (
	"github.com/BrokkAi/mjolnir/go/rs"
)

func parseDocument(raw string) (Document, error) {
	b := newInput(raw)
	doc, e := document(b)
	if tomlError := finishParse(b, e); tomlError != nil {
		return Document{}, tomlError
	}
	doc.span = rs.Some(Range{Start: 0, End: len(raw)})
	doc.original = rs.Some(raw)
	return doc, nil
}

func parseKey(raw string) (Key, error) {
	b := newInput(raw)
	rawKey, key, e := simpleKey(b)
	if tomlError := finishParse(b, e); tomlError != nil {
		return Key{}, tomlError
	}
	return KeyNew(key).withReprUnchecked(reprNewUnchecked(rawKey)), nil
}

func parseKeyPath(raw string) ([]Key, error) {
	b := newInput(raw)
	keys, e := key(b)
	if tomlError := finishParse(b, e); tomlError != nil {
		return nil, tomlError
	}
	for index := range keys {
		keys[index].despan(raw)
	}
	return keys, nil
}

func parseValue(raw string) (Value, error) {
	b := newInput(raw)
	parsed, e := value(b, recursionCheck{})
	if tomlError := finishParse(b, e); tomlError != nil {
		return nil, tomlError
	}
	// Only take the repr and not decor, as its probably not intended
	parsed.DecorMut().Clear()
	parsed.despan(raw)
	return parsed, nil
}

// finishParse is the rest of winnow's `Parser::parse` after the parser has
// run: the whole input must have been consumed, and a failure becomes a
// TomlError pointing at where the input was left. It returns nil on success.
func finishParse(b *input, e parseErr) *TomlError {
	if !e.failed() && b.eofOffset() != 0 {
		// (self.by_ref(), eof).parse_next(&mut input)
		e = backtrack()
	}
	if !e.failed() {
		return nil
	}
	return tomlErrorNew(e, b)
}

// recursionCheck is parser::prelude::RecursionCheck, as built without the
// `unbounded` feature.
type recursionCheck struct {
	current int
}

func recursionCheckDepth(depth int) error {
	if depth < 128 {
		return nil
	}
	return &customError_RecursionLimitExceeded{}
}

func (c recursionCheck) recursing() (recursionCheck, parseErr) {
	c.current++
	if c.current < 128 {
		return c, parseErr{}
	}
	return c, external(&customError_RecursionLimitExceeded{})
}
