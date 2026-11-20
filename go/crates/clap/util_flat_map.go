// Port of clap_builder 4.6.0 src/util/flat_map.rs

package clap

// utilFlatMap is clap's FlatMap: a flat (Vec) backed map.
//
// This preserves insertion order.
type utilFlatMap[K comparable, V any] struct {
	keys   []K
	values []V
}

func (m *utilFlatMap[K, V]) insert(key K, value V) (V, bool) {
	for index, existing := range m.keys {
		if existing == key {
			old := m.values[index]
			m.values[index] = value
			return old, true
		}
	}

	m.insertUnchecked(key, value)
	var zero V
	return zero, false
}

func (m *utilFlatMap[K, V]) insertUnchecked(key K, value V) {
	m.keys = append(m.keys, key)
	m.values = append(m.values, value)
}

func (m *utilFlatMap[K, V]) containsKey(key K) bool {
	for _, existing := range m.keys {
		if existing == key {
			return true
		}
	}
	return false
}

func (m *utilFlatMap[K, V]) remove(key K) (V, bool) {
	_, value, ok := m.removeEntry(key)
	return value, ok
}

func (m *utilFlatMap[K, V]) removeEntry(key K) (K, V, bool) {
	for index, existing := range m.keys {
		if existing == key {
			value := m.values[index]
			// Vec::remove: the entries after it keep their order.
			m.keys = append(m.keys[:index:index], m.keys[index+1:]...)
			m.values = append(m.values[:index:index], m.values[index+1:]...)
			return existing, value, true
		}
	}
	var zeroK K
	var zeroV V
	return zeroK, zeroV, false
}

func (m *utilFlatMap[K, V]) isEmpty() bool {
	return len(m.keys) == 0
}

// get returns a pointer to the value stored under k, or nil.
func (m *utilFlatMap[K, V]) get(k K) *V {
	for index, existing := range m.keys {
		if existing == k {
			return &m.values[index]
		}
	}
	return nil
}

// entryOrInsert is `entry(key).or_insert(default)`: a pointer to the value
// stored under key, storing def first when there is none.
func (m *utilFlatMap[K, V]) entryOrInsert(key K, def V) *V {
	for index, existing := range m.keys {
		if existing == key {
			return &m.values[index]
		}
	}
	m.keys = append(m.keys, key)
	m.values = append(m.values, def)
	return &m.values[len(m.values)-1]
}

func (m *utilFlatMap[K, V]) len() int { return len(m.keys) }
