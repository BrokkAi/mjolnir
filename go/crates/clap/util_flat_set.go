// Port of clap_builder 4.6.0 src/util/flat_set.rs

package clap

// utilFlatSet is clap's FlatSet: a flat (Vec) backed set.
//
// This preserves insertion order.
type utilFlatSet[T comparable] struct {
	inner []T
}

func (s *utilFlatSet[T]) insert(value T) bool {
	for _, existing := range s.inner {
		if existing == value {
			return false
		}
	}
	s.inner = append(s.inner, value)
	return true
}

func (s *utilFlatSet[T]) contains(value T) bool {
	for _, existing := range s.inner {
		if existing == value {
			return true
		}
	}
	return false
}

func (s *utilFlatSet[T]) isEmpty() bool {
	return len(s.inner) == 0
}

func (s *utilFlatSet[T]) extend(values []T) {
	for _, value := range values {
		s.insert(value)
	}
}

func (s *utilFlatSet[T]) intoVec() []T {
	return s.inner
}
