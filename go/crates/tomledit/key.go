// Port of toml_edit 0.20.2 src/key.rs

package tomledit

import (
	"cmp"
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Key is toml_edit::Key: a key as part of a Key/Value Pair or a table header.
//
// # Examples
//
//	[dependencies."nom"]
//	version = "5.0"
//	'literal key' = "nonsense"
//	"basic string key" = 42
//
// There are 3 types of keys:
//
//  1. Bare keys (`version` and `dependencies`)
//
//  2. Basic quoted keys (`"basic string key"` and `"nom"`)
//
//  3. Literal quoted keys (`'literal key'`)
//
// For details see [toml spec](https://github.com/toml-lang/toml/#keyvalue-pair).
//
// To parse a key use `FromStr` trait implementation: `"string".parse::<Key>()`.
type Key struct {
	key   InternalString
	repr  rs.Option[Repr]
	decor Decor
}

// KeyNew is Key::new: create a new table key.
func KeyNew(key string) Key {
	return Key{key: key}
}

// KeyParse is Key::parse: parse a TOML key expression.
//
// Unlike `"".parse<Key>()`, this supports dotted keys.
func KeyParse(repr string) ([]Key, error) {
	return keyTryParsePath(repr)
}

func (k Key) withReprUnchecked(repr Repr) Key {
	k.repr = rs.Some(repr)
	return k
}

// WithDecor is Key::with_decor: while creating the `Key`, add `Decor` to it.
func (k Key) WithDecor(decor Decor) Key {
	k.decor = decor
	return k
}

// AsMut is Key::as_mut: access a mutable proxy for parts of the key.
func (k *Key) AsMut() KeyMut {
	return KeyMut{key: k}
}

// Get is Key::get: the parsed key value.
func (k *Key) Get() string { return k.key }

func (k *Key) getInternal() InternalString { return k.key }

// AsRepr is Key::as_repr: the key raw representation, if available.
func (k *Key) AsRepr() rs.Option[*Repr] {
	if k.repr.IsSome() {
		return rs.Some(k.repr.AsRef())
	}
	return rs.None[*Repr]()
}

// DefaultRepr is Key::default_repr: the default raw representation.
func (k *Key) DefaultRepr() Repr {
	return toKeyRepr(k.key)
}

// DisplayRepr is Key::display_repr: a raw representation.
func (k *Key) DisplayRepr() string {
	if repr, ok := k.repr.Get(); ok {
		if text, ok := repr.rawValue.AsStr().Get(); ok {
			return text
		}
	}
	return k.DefaultRepr().rawValue.AsStr().Unwrap()
}

// DecorMut is Key::decor_mut: the surrounding whitespace.
func (k *Key) DecorMut() *Decor { return &k.decor }

// Decor is Key::decor: the surrounding whitespace.
func (k *Key) Decor() *Decor { return &k.decor }

// span is Key::span: the location within the original document.
func (k *Key) span() rs.Option[Range] {
	if repr, ok := k.repr.Get(); ok {
		return repr.span()
	}
	return rs.None[Range]()
}

func (k *Key) despan(input string) {
	k.decor.despan(input)
	if repr := k.repr.AsRef(); repr != nil {
		repr.despan(input)
	}
}

// Fmt is Key::fmt: auto formats the key.
func (k *Key) Fmt() {
	k.repr = rs.Some(toKeyRepr(k.key))
	k.decor.Clear()
}

func keyTryParseSimple(s string) (Key, error) {
	key, err := parseKey(s)
	if err != nil {
		return Key{}, err
	}
	key.despan(s)
	return key, nil
}

func keyTryParsePath(s string) ([]Key, error) {
	keys, err := parseKeyPath(s)
	if err != nil {
		return nil, err
	}
	for index := range keys {
		keys[index].despan(s)
	}
	return keys, nil
}

// Clone is `impl Clone for Key`.
func (k *Key) Clone() Key { return *k }

// Deref is `impl Deref<Target = str> for Key`.
func (k *Key) Deref() string { return k.Get() }

// HashKey is `impl Hash for Key`: a key hashes as its value.
func (k *Key) HashKey() string { return k.Get() }

// Compare is `impl Ord for Key`: keys order by their value.
func (k *Key) Compare(other *Key) int {
	return cmp.Compare(k.Get(), other.Get())
}

// Equal is `impl PartialEq for Key`: keys are equal when their values are,
// whatever their representation and decor.
func (k *Key) Equal(other *Key) bool {
	return k.Get() == other.Get()
}

// EqualStr is `impl PartialEq<str> for Key` (and for &str and String).
func (k *Key) EqualStr(other string) bool {
	return k.Get() == other
}

// String is `impl Display for Key`.
func (k *Key) String() string {
	var buf strings.Builder
	encodeKey(k, &buf, rs.None[string](), "", "")
	return buf.String()
}

// FmtDisplay is `impl Display for Key`.
func (k *Key) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(k.String()) }

// FmtDebug is the derived `Debug for Key`.
func (k *Key) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("Key").Field("key", k.key).Field("repr", k.repr).Field("decor", k.decor).Finish()
}

// KeyFromStr is `impl FromStr for Key`.
//
// Tries to parse a key from a &str, if fails, tries as basic quoted key
// (surrounds with "") and then literal quoted key (surrounds with ”).
func KeyFromStr(s string) (Key, error) {
	return keyTryParseSimple(s)
}

func toKeyRepr(key string) Repr {
	allUnquoted := true
	for index := 0; index < len(key); index++ {
		if !isUnquotedChar(key[index]) {
			allUnquoted = false
			break
		}
	}
	if allUnquoted && key != "" {
		return reprNewUnchecked(RawStringFromString(key))
	}
	return toStringRepr(key, rs.Some(stringStyleOnelineSingle), rs.Some(false))
}

// KeyFromString is `impl From<&str> for Key` (and for String, &String and
// InternalString).
func KeyFromString(s string) Key {
	return KeyNew(s)
}

// intoKey is `impl Into<Key>`: a string or a Key.
func intoKey(value any) Key {
	switch v := value.(type) {
	case Key:
		return v
	case *Key:
		return *v
	case string:
		return KeyNew(v)
	default:
		panic(rs.Unreachable("toml_edit: Into<Key> is implemented for strings and Key only"))
	}
}

// KeyMut is toml_edit::KeyMut: a mutable reference to a `Key`.
type KeyMut struct {
	key *Key
}

// Get is KeyMut::get: the parsed key value.
func (k KeyMut) Get() string { return k.key.Get() }

// AsRepr is KeyMut::as_repr: the raw representation, if available.
func (k KeyMut) AsRepr() rs.Option[*Repr] { return k.key.AsRepr() }

// DefaultRepr is KeyMut::default_repr: the default raw representation.
func (k KeyMut) DefaultRepr() Repr { return k.key.DefaultRepr() }

// DisplayRepr is KeyMut::display_repr: a raw representation.
func (k KeyMut) DisplayRepr() string { return k.key.DisplayRepr() }

// DecorMut is KeyMut::decor_mut: the surrounding whitespace.
func (k KeyMut) DecorMut() *Decor { return k.key.DecorMut() }

// Decor is KeyMut::decor: the surrounding whitespace.
func (k KeyMut) Decor() *Decor { return k.key.Decor() }

// Fmt is KeyMut::fmt: auto formats the key.
func (k KeyMut) Fmt() { k.key.Fmt() }

// Deref is `impl Deref<Target = str> for KeyMut`.
func (k KeyMut) Deref() string { return k.Get() }

// EqualStr is `impl PartialEq<str> for KeyMut` (and for &str and String).
func (k KeyMut) EqualStr(other string) bool { return k.Get() == other }

// Equal is the derived `PartialEq for KeyMut`.
func (k KeyMut) Equal(other KeyMut) bool { return k.key.Equal(other.key) }

// Compare is the derived `Ord for KeyMut`.
func (k KeyMut) Compare(other KeyMut) int { return k.key.Compare(other.key) }

// String is `impl Display for KeyMut`.
func (k KeyMut) String() string { return k.key.String() }

// FmtDisplay is `impl Display for KeyMut`.
func (k KeyMut) FmtDisplay(f *rs.Formatter) error { return k.key.FmtDisplay(f) }
