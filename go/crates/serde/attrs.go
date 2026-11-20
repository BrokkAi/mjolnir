// Port of serde_derive 1.0.229 src/internals/attr.rs (the attribute grammar,
// as the port spells it), src/internals/case.rs and src/internals/name.rs

package serde

import (
	"slices"
	"strconv"
	"strings"
	"unicode"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// attribute is one serde attribute: `name`, `name = "value"`, or
// `name(inner, ..)` such as `rename(serialize = "a", deserialize = "b")`.
type attribute struct {
	name     string
	value    string
	hasValue bool
	inner    []attribute
}

// parseAttributes reads a list of serde attributes separated by commas, in
// the grammar doc.go gives: the content of a `serde:"..."` struct tag, or one
// entry of a registration's attributes.
func parseAttributes(text string, where string) []attribute {
	p := attributeParser{text: text, where: where}
	list := p.list(false)
	if p.pos < len(p.text) {
		p.fail("unexpected `" + p.text[p.pos:] + "`")
	}
	return list
}

type attributeParser struct {
	text  string
	pos   int
	where string
}

func (p *attributeParser) fail(what string) {
	panic(rs.Panicf("serde: malformed attributes `" + p.text + "` on " + p.where + ": " + what))
}

func (p *attributeParser) space() {
	for p.pos < len(p.text) && (p.text[p.pos] == ' ' || p.text[p.pos] == '\t' || p.text[p.pos] == '\n') {
		p.pos++
	}
}

func (p *attributeParser) list(nested bool) []attribute {
	var list []attribute
	for {
		p.space()
		if p.pos >= len(p.text) || (nested && p.text[p.pos] == ')') {
			return list
		}
		list = append(list, p.one())
		p.space()
		if p.pos < len(p.text) && p.text[p.pos] == ',' {
			p.pos++
			continue
		}
		return list
	}
}

func (p *attributeParser) one() attribute {
	start := p.pos
	for p.pos < len(p.text) {
		c := p.text[p.pos]
		if c == '_' || (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') {
			p.pos++
			continue
		}
		break
	}
	if p.pos == start {
		p.fail("expected an attribute name")
	}
	attr := attribute{name: p.text[start:p.pos]}
	p.space()
	if p.pos < len(p.text) && p.text[p.pos] == '(' {
		p.pos++
		attr.inner = p.list(true)
		p.space()
		if p.pos >= len(p.text) || p.text[p.pos] != ')' {
			p.fail("expected `)`")
		}
		p.pos++
		return attr
	}
	if p.pos < len(p.text) && p.text[p.pos] == '=' {
		p.pos++
		p.space()
		attr.hasValue = true
		attr.value = p.value()
	}
	return attr
}

// value reads a value: a double-quoted Go string, or bare text up to the
// next comma or closing parenthesis that is not inside brackets (so that a
// type such as `BTreeMap<String, PathBuf>` needs no quotes).
func (p *attributeParser) value() string {
	if p.pos < len(p.text) && p.text[p.pos] == '"' {
		end := p.pos + 1
		for end < len(p.text) && p.text[end] != '"' {
			if p.text[end] == '\\' {
				end++
			}
			end++
		}
		if end >= len(p.text) {
			p.fail("unterminated string")
		}
		value, err := strconv.Unquote(p.text[p.pos : end+1])
		if err != nil {
			p.fail("bad string " + p.text[p.pos:end+1])
		}
		p.pos = end + 1
		return value
	}
	start := p.pos
	depth := 0
	for p.pos < len(p.text) {
		switch p.text[p.pos] {
		case '<', '(', '[':
			depth++
		case '>', ']':
			depth--
		case ')':
			if depth == 0 {
				return strings.TrimSpace(p.text[start:p.pos])
			}
			depth--
		case ',':
			if depth == 0 {
				return strings.TrimSpace(p.text[start:p.pos])
			}
		}
		p.pos++
	}
	return strings.TrimSpace(p.text[start:p.pos])
}

// find returns the last attribute with the name, as serde reports a
// duplicate at compile time and the port has only one.
func findAttribute(attrs []attribute, name string) (attribute, bool) {
	for index := len(attrs) - 1; index >= 0; index-- {
		if attrs[index].name == name {
			return attrs[index], true
		}
	}
	return attribute{}, false
}

func hasAttribute(attrs []attribute, name string) bool {
	_, ok := findAttribute(attrs, name)
	return ok
}

// serDeValue reads an attribute that names one value for both directions
// (`rename = "a"`) or one for each (`rename(serialize = "a", deserialize =
// "b")`).
func serDeValue(attrs []attribute, name string) (ser string, hasSer bool, de string, hasDe bool) {
	for _, attr := range attrs {
		if attr.name != name {
			continue
		}
		if attr.hasValue {
			ser, hasSer, de, hasDe = attr.value, true, attr.value, true
			continue
		}
		for _, inner := range attr.inner {
			switch inner.name {
			case "serialize":
				ser, hasSer = inner.value, true
			case "deserialize":
				de, hasDe = inner.value, true
			}
		}
	}
	return ser, hasSer, de, hasDe
}

// renameRule is `RenameRule`: the different possible ways to change case of
// fields in a struct, or variants in an enum.
type renameRule uint8

const (
	// renameNone doesn't apply a default rename rule.
	renameNone renameRule = iota
	// renameLowerCase renames direct children to "lowercase" style.
	renameLowerCase
	// renameUpperCase renames direct children to "UPPERCASE" style.
	renameUpperCase
	// renamePascalCase renames direct children to "PascalCase" style, as
	// typically used for enum variants.
	renamePascalCase
	// renameCamelCase renames direct children to "camelCase" style.
	renameCamelCase
	// renameSnakeCase renames direct children to "snake_case" style, as
	// commonly used for fields.
	renameSnakeCase
	// renameScreamingSnakeCase renames direct children to
	// "SCREAMING_SNAKE_CASE" style, as commonly used for constants.
	renameScreamingSnakeCase
	// renameKebabCase renames direct children to "kebab-case" style.
	renameKebabCase
	// renameScreamingKebabCase renames direct children to
	// "SCREAMING-KEBAB-CASE" style.
	renameScreamingKebabCase
)

var renameRules = []struct {
	name string
	rule renameRule
}{
	{"lowercase", renameLowerCase},
	{"UPPERCASE", renameUpperCase},
	{"PascalCase", renamePascalCase},
	{"camelCase", renameCamelCase},
	{"snake_case", renameSnakeCase},
	{"SCREAMING_SNAKE_CASE", renameScreamingSnakeCase},
	{"kebab-case", renameKebabCase},
	{"SCREAMING-KEBAB-CASE", renameScreamingKebabCase},
}

func parseRenameRule(text string, where string) renameRule {
	for _, known := range renameRules {
		if text == known.name {
			return known.rule
		}
	}
	names := make([]string, len(renameRules))
	for index, known := range renameRules {
		names[index] = DebugStr(known.name)
	}
	panic(rs.Panicf("serde: " + where + ": unknown rename rule `rename_all = " + DebugStr(text) + "`, expected one of " + strings.Join(names, ", ")))
}

// asciiLower and asciiUpper are `to_ascii_lowercase` and
// `to_ascii_uppercase`.
func asciiLower(s string) string {
	return strings.Map(func(r rune) rune {
		if r >= 'A' && r <= 'Z' {
			return r + 'a' - 'A'
		}
		return r
	}, s)
}

func asciiUpper(s string) string {
	return strings.Map(func(r rune) rune {
		if r >= 'a' && r <= 'z' {
			return r - 'a' + 'A'
		}
		return r
	}, s)
}

// applyToVariant applies a renaming rule to an enum variant, returning the
// version expected in the source.
func (rule renameRule) applyToVariant(variant string) string {
	switch rule {
	case renameNone, renamePascalCase:
		return variant
	case renameLowerCase:
		return asciiLower(variant)
	case renameUpperCase:
		return asciiUpper(variant)
	case renameCamelCase:
		return asciiLower(variant[:1]) + variant[1:]
	case renameSnakeCase:
		var snake strings.Builder
		for index, ch := range variant {
			if index > 0 && unicode.IsUpper(ch) {
				snake.WriteByte('_')
			}
			if ch >= 'A' && ch <= 'Z' {
				ch += 'a' - 'A'
			}
			snake.WriteRune(ch)
		}
		return snake.String()
	case renameScreamingSnakeCase:
		return asciiUpper(renameSnakeCase.applyToVariant(variant))
	case renameKebabCase:
		return strings.ReplaceAll(renameSnakeCase.applyToVariant(variant), "_", "-")
	case renameScreamingKebabCase:
		return strings.ReplaceAll(renameScreamingSnakeCase.applyToVariant(variant), "_", "-")
	}
	panic(rs.Unreachable(rule))
}

// applyToField applies a renaming rule to a struct field, returning the
// version expected in the source.
func (rule renameRule) applyToField(field string) string {
	switch rule {
	case renameNone, renameLowerCase, renameSnakeCase:
		return field
	case renameUpperCase:
		return asciiUpper(field)
	case renamePascalCase:
		var pascal strings.Builder
		capitalize := true
		for _, ch := range field {
			switch {
			case ch == '_':
				capitalize = true
			case capitalize:
				if ch >= 'a' && ch <= 'z' {
					ch -= 'a' - 'A'
				}
				pascal.WriteRune(ch)
				capitalize = false
			default:
				pascal.WriteRune(ch)
			}
		}
		return pascal.String()
	case renameCamelCase:
		pascal := renamePascalCase.applyToField(field)
		return asciiLower(pascal[:1]) + pascal[1:]
	case renameScreamingSnakeCase:
		return asciiUpper(field)
	case renameKebabCase:
		return strings.ReplaceAll(field, "_", "-")
	case renameScreamingKebabCase:
		return strings.ReplaceAll(renameScreamingSnakeCase.applyToField(field), "_", "-")
	}
	panic(rs.Unreachable(rule))
}

// or is `RenameRule::or`: this rule, or rule_b when this one is None.
func (rule renameRule) or(other renameRule) renameRule {
	if rule == renameNone {
		return other
	}
	return rule
}

// renameAllRules is `RenameAllRules`: the rule for each direction.
type renameAllRules struct {
	serialize   renameRule
	deserialize renameRule
}

func (r renameAllRules) or(other renameAllRules) renameAllRules {
	return renameAllRules{serialize: r.serialize.or(other.serialize), deserialize: r.deserialize.or(other.deserialize)}
}

func parseRenameAll(attrs []attribute, name string, where string) renameAllRules {
	ser, hasSer, de, hasDe := serDeValue(attrs, name)
	var rules renameAllRules
	if hasSer {
		rules.serialize = parseRenameRule(ser, where)
	}
	if hasDe {
		rules.deserialize = parseRenameRule(de, where)
	}
	return rules
}

// multiName is `MultiName`: the names of a container, a variant or a field
// when it is serialized and when it is deserialized.
type multiName struct {
	serialize          string
	serializeRenamed   bool
	deserialize        string
	deserializeRenamed bool
	// aliases holds the alias attributes until the rename rules have been
	// applied, and then the names a deserializer accepts: the aliases and
	// the deserialize name, in the order of Rust's BTreeSet (by bytes),
	// without repeats.
	aliases []string
}

func newMultiName(sourceName string, attrs []attribute) multiName {
	name := multiName{serialize: sourceName, deserialize: sourceName}
	if ser, hasSer, de, hasDe := serDeValue(attrs, "rename"); hasSer || hasDe {
		if hasSer {
			name.serialize, name.serializeRenamed = ser, true
		}
		if hasDe {
			name.deserialize, name.deserializeRenamed = de, true
		}
	}
	for _, attr := range attrs {
		if attr.name == "alias" {
			name.aliases = append(name.aliases, attr.value)
		}
	}
	return name
}

// finish adds the deserialize name to the aliases, as `rename_by_rules`
// does after it applied the rules, and puts them in BTreeSet order.
func (name *multiName) finish() {
	name.aliases = append(name.aliases, name.deserialize)
	slices.Sort(name.aliases)
	name.aliases = slices.Compact(name.aliases)
}

func (name *multiName) renameVariantByRules(rules renameAllRules) {
	if !name.serializeRenamed {
		name.serialize = rules.serialize.applyToVariant(name.serialize)
	}
	if !name.deserializeRenamed {
		name.deserialize = rules.deserialize.applyToVariant(name.deserialize)
	}
	name.finish()
}

func (name *multiName) renameFieldByRules(rules renameAllRules) {
	if !name.serializeRenamed {
		name.serialize = rules.serialize.applyToField(name.serialize)
	}
	if !name.deserializeRenamed {
		name.deserialize = rules.deserialize.applyToField(name.deserialize)
	}
	name.finish()
}

// unraw is `unraw`: an identifier without the `r#` of a raw identifier.
func unraw(ident string) string { return strings.TrimPrefix(ident, "r#") }

// rustType is a Rust type as written in a field declaration, parsed far
// enough to tell apart the Rust types that share a Go type: `char` and
// `i32` (both int32), `usize` and `isize` (both int), `String` and `PathBuf`
// (both string).
type rustType struct {
	// name is the last segment of the type's path (`PathBuf`, `Option`,
	// `BTreeMap`), "()" for the unit type, "tuple" for a tuple, "array" for
	// `[T; N]` and "slice" for `[T]`.
	name string
	// args are the generic arguments, the elements of a tuple, or the element
	// of an array or slice.
	args []*rustType
}

// parseRustType parses a field's Rust type. It returns nil for text it does
// not understand, which leaves every Go type with its default meaning.
func parseRustType(text string) *rustType {
	p := rustTypeParser{text: text}
	t := p.parse()
	p.space()
	if !p.ok || p.pos != len(p.text) {
		return nil
	}
	return t
}

type rustTypeParser struct {
	text string
	pos  int
	ok   bool
}

func (p *rustTypeParser) space() {
	for p.pos < len(p.text) && (p.text[p.pos] == ' ' || p.text[p.pos] == '\t' || p.text[p.pos] == '\n') {
		p.pos++
	}
}

func (p *rustTypeParser) eat(prefix string) bool {
	p.space()
	if strings.HasPrefix(p.text[p.pos:], prefix) {
		p.pos += len(prefix)
		return true
	}
	return false
}

func (p *rustTypeParser) parse() *rustType {
	p.ok = true
	return p.one()
}

func (p *rustTypeParser) one() *rustType {
	p.space()
	// References and lifetimes do not change how a type is serialized.
	for p.eat("&") {
		p.space()
		if p.pos < len(p.text) && p.text[p.pos] == '\'' {
			p.ident()
		}
		p.eat("mut ")
	}
	p.eat("dyn ")
	switch {
	case p.eat("("):
		tuple := &rustType{name: "tuple"}
		for {
			if p.eat(")") {
				break
			}
			tuple.args = append(tuple.args, p.one())
			if !p.eat(",") {
				if !p.eat(")") {
					p.ok = false
				}
				break
			}
		}
		if len(tuple.args) == 0 {
			tuple.name = "()"
		}
		return tuple
	case p.eat("["):
		element := p.one()
		if p.eat(";") {
			for p.pos < len(p.text) && p.text[p.pos] != ']' {
				p.pos++
			}
			if !p.eat("]") {
				p.ok = false
			}
			return &rustType{name: "array", args: []*rustType{element}}
		}
		if !p.eat("]") {
			p.ok = false
		}
		return &rustType{name: "slice", args: []*rustType{element}}
	}
	t := &rustType{}
	for {
		p.eat("::")
		name := p.ident()
		if name == "" {
			p.ok = false
			return t
		}
		t.name = name
		t.args = nil
		if p.eat("<") {
			for {
				if p.eat(">") {
					break
				}
				p.space()
				if p.pos < len(p.text) && p.text[p.pos] == '\'' {
					// A lifetime argument.
					p.ident()
				} else {
					t.args = append(t.args, p.one())
				}
				if !p.eat(",") {
					if !p.eat(">") {
						p.ok = false
					}
					break
				}
			}
		}
		p.space()
		if !strings.HasPrefix(p.text[p.pos:], "::") {
			return t
		}
	}
}

func (p *rustTypeParser) ident() string {
	p.space()
	start := p.pos
	if p.pos < len(p.text) && p.text[p.pos] == '\'' {
		p.pos++
	}
	for p.pos < len(p.text) {
		r, size := utf8.DecodeRuneInString(p.text[p.pos:])
		if r == '_' || unicode.IsLetter(r) || unicode.IsDigit(r) || (r == '#' && p.pos > start) {
			p.pos += size
			continue
		}
		break
	}
	return p.text[start:p.pos]
}

// arg returns the type's index-th argument, or nil.
func (t *rustType) arg(index int) *rustType {
	if t == nil || index >= len(t.args) {
		return nil
	}
	return t.args[index]
}

// through returns the type behind the wrappers that Go does not have: the
// T of a `Box<T>`, `Arc<T>`, `Rc<T>` or `Cow<T>` when the Go type is not a
// pointer.
func (t *rustType) peel() *rustType {
	for t != nil && len(t.args) == 1 {
		switch t.name {
		case "Box", "Arc", "Rc", "Cow":
			t = t.args[0]
			continue
		}
		break
	}
	return t
}
