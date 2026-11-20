// Port of clap_builder 4.6.0 src/mkeymap.rs

package clap

import "github.com/BrokkAi/mjolnir/go/rs"

type mkeymapKey struct {
	key   mkeymapKeyType
	index int
}

// mkeymapMKeyMap is MKeyMap.
type mkeymapMKeyMap struct {
	// All of the arguments.
	args []*Arg

	// Cache part:
	// Will be set after `_build()`.
	keys []mkeymapKey
}

// mkeymapKeyKind says which variant of KeyType a key is.
type mkeymapKeyKind uint8

const (
	mkeymapKeyType_Short mkeymapKeyKind = iota
	mkeymapKeyType_Long
	mkeymapKeyType_Position
)

// mkeymapKeyType is KeyType: Short(char), Long(OsStr) or Position(usize).
type mkeymapKeyType struct {
	kind     mkeymapKeyKind
	short    rune
	long     string
	position int
}

func (k mkeymapKeyType) isPosition() bool {
	return k.kind == mkeymapKeyType_Position
}

func (k mkeymapKeyType) eqPosition(rhs int) bool {
	return k.kind == mkeymapKeyType_Position && k.position == rhs
}

func (k mkeymapKeyType) eqLong(rhs string) bool {
	return k.kind == mkeymapKeyType_Long && k.long == rhs
}

func (k mkeymapKeyType) eqShort(rhs rune) bool {
	return k.kind == mkeymapKeyType_Short && k.short == rhs
}

// containsShort reports whether any arg has this short flag as a key.
func (m *mkeymapMKeyMap) containsShort(key rune) bool {
	for i := range m.keys {
		if m.keys[i].key.eqShort(key) {
			return true
		}
	}
	return false
}

// push pushes an argument in the map.
func (m *mkeymapMKeyMap) push(newArg *Arg) {
	m.args = append(m.args, newArg)
}

// getPosition finds the arg that has the positional index as a key.
func (m *mkeymapMKeyMap) getPosition(key int) *Arg {
	for i := range m.keys {
		if m.keys[i].key.eqPosition(key) {
			return m.args[m.keys[i].index]
		}
	}
	return nil
}

// getShort finds the arg that has the short flag as a key.
func (m *mkeymapMKeyMap) getShort(key rune) *Arg {
	for i := range m.keys {
		if m.keys[i].key.eqShort(key) {
			return m.args[m.keys[i].index]
		}
	}
	return nil
}

// getLong finds the arg that has the long flag as a key.
func (m *mkeymapMKeyMap) getLong(key string) *Arg {
	for i := range m.keys {
		if m.keys[i].key.eqLong(key) {
			return m.args[m.keys[i].index]
		}
	}
	return nil
}

// mutArgs mutates every argument.
func (m *mkeymapMKeyMap) mutArgs(f func(*Arg) *Arg) {
	args := m.args
	m.args = nil
	for _, arg := range args {
		m.args = append(m.args, f(arg))
	}
}

// build: we need a lazy build here since some we may change args after
// creating the map, you can checkout who uses `args_mut`.
func (m *mkeymapMKeyMap) build() {
	// There will be at least as many keys as args, so that is a good starting point
	for i, arg := range m.args {
		m.keys = mkeymapAppendKeys(m.keys, arg, i)
	}
}

// removeByName removes an arg in the graph by Id, usually used by
// `mut_arg`. Return the arg if removed.
func (m *mkeymapMKeyMap) removeByName(name string) rs.Option[*Arg] {
	for i, arg := range m.args {
		if arg.id == Id(name) {
			// since it's a cold function, using this wouldn't hurt much
			m.args = append(m.args[:i:i], m.args[i+1:]...)
			return rs.Some(arg)
		}
	}
	return rs.None[*Arg]()
}

// clone is the derived Clone of MKeyMap.
func (m *mkeymapMKeyMap) clone() mkeymapMKeyMap {
	cloned := mkeymapMKeyMap{
		args: make([]*Arg, len(m.args)),
		keys: append([]mkeymapKey(nil), m.keys...),
	}
	for i, arg := range m.args {
		cloned.args[i] = arg.Clone()
	}
	return cloned
}

// mkeymapAppendKeys generates key types for a specific Arg.
func mkeymapAppendKeys(keys []mkeymapKey, arg *Arg, index int) []mkeymapKey {
	if posIndex, ok := arg.index.Get(); ok {
		key := mkeymapKeyType{kind: mkeymapKeyType_Position, position: posIndex}
		keys = append(keys, mkeymapKey{key: key, index: index})
	} else {
		if short, ok := arg.short.Get(); ok {
			key := mkeymapKeyType{kind: mkeymapKeyType_Short, short: short}
			keys = append(keys, mkeymapKey{key: key, index: index})
		}
		if long, ok := arg.long.Get(); ok {
			key := mkeymapKeyType{kind: mkeymapKeyType_Long, long: long}
			keys = append(keys, mkeymapKey{key: key, index: index})
		}

		for _, alias := range arg.shortAliases {
			key := mkeymapKeyType{kind: mkeymapKeyType_Short, short: alias.name}
			keys = append(keys, mkeymapKey{key: key, index: index})
		}
		for _, alias := range arg.aliases {
			key := mkeymapKeyType{kind: mkeymapKeyType_Long, long: alias.name}
			keys = append(keys, mkeymapKey{key: key, index: index})
		}
	}
	return keys
}
