// Port of semver 1.0.28 src/impls.rs

package semver

import (
	"cmp"
	"strings"
)

// `impl Default`, `Eq` and `Hash` for Identifier are those of the string it
// is here.

// Deref is `impl Deref for Prerelease`: the identifier as a str.
func (self *Prerelease) Deref() string {
	return self.identifier.asStr()
}

// Deref is `impl Deref for BuildMetadata`: the identifier as a str.
func (self *BuildMetadata) Deref() string {
	return self.identifier.asStr()
}

// implsIsAsciiDigits is `text.bytes().all(|b| b.is_ascii_digit())`.
func implsIsAsciiDigits(text string) bool {
	for i := 0; i < len(text); i++ {
		if text[i] < '0' || text[i] > '9' {
			return false
		}
	}
	return true
}

// Compare is `impl Ord for Prerelease`: negative, zero or positive.
func (self *Prerelease) Compare(rhs *Prerelease) int {
	if self.identifier.ptrEq(rhs.identifier) {
		return 0
	}

	switch {
	// A real release compares greater than prerelease.
	case self.IsEmpty():
		return 1
	// Prerelease compares less than the real release.
	case rhs.IsEmpty():
		return -1
	}

	lhsParts := strings.Split(self.AsStr(), ".")
	rhsParts := strings.Split(rhs.AsStr(), ".")
	rhsNext := 0

	for _, lhs := range lhsParts {
		if rhsNext >= len(rhsParts) {
			// Spec: "A larger set of pre-release fields has a higher
			// precedence than a smaller set, if all of the preceding
			// identifiers are equal."
			return 1
		}
		rhs := rhsParts[rhsNext]
		rhsNext++

		stringCmp := func() int { return cmp.Compare(lhs, rhs) }
		var ordering int
		lhsDigits, rhsDigits := implsIsAsciiDigits(lhs), implsIsAsciiDigits(rhs)
		switch {
		// Respect numeric ordering, for example 99 < 100. Spec says:
		// "Identifiers consisting of only digits are compared
		// numerically."
		case lhsDigits && rhsDigits:
			ordering = cmp.Compare(len(lhs), len(rhs))
			if ordering == 0 {
				ordering = stringCmp()
			}
		// Spec: "Numeric identifiers always have lower precedence than
		// non-numeric identifiers."
		case lhsDigits && !rhsDigits:
			return -1
		case !lhsDigits && rhsDigits:
			return 1
		// Spec: "Identifiers with letters or hyphens are compared
		// lexically in ASCII sort order."
		default:
			ordering = stringCmp()
		}

		if ordering != 0 {
			return ordering
		}
	}

	if rhsNext >= len(rhsParts) {
		return 0
	} else {
		return -1
	}
}

// Compare is `impl Ord for BuildMetadata`: negative, zero or positive.
func (self *BuildMetadata) Compare(rhs *BuildMetadata) int {
	if self.identifier.ptrEq(rhs.identifier) {
		return 0
	}

	lhsParts := strings.Split(self.AsStr(), ".")
	rhsParts := strings.Split(rhs.AsStr(), ".")
	rhsNext := 0

	for _, lhs := range lhsParts {
		if rhsNext >= len(rhsParts) {
			return 1
		}
		rhs := rhsParts[rhsNext]
		rhsNext++

		var ordering int
		lhsDigits, rhsDigits := implsIsAsciiDigits(lhs), implsIsAsciiDigits(rhs)
		switch {
		case lhsDigits && rhsDigits:
			// 0 < 00 < 1 < 01 < 001 < 2 < 02 < 002 < 10
			lhval := strings.TrimLeft(lhs, "0")
			rhval := strings.TrimLeft(rhs, "0")
			ordering = cmp.Compare(len(lhval), len(rhval))
			if ordering == 0 {
				ordering = cmp.Compare(lhval, rhval)
			}
			if ordering == 0 {
				ordering = cmp.Compare(len(lhs), len(rhs))
			}
		case lhsDigits && !rhsDigits:
			return -1
		case !lhsDigits && rhsDigits:
			return 1
		default:
			ordering = cmp.Compare(lhs, rhs)
		}

		if ordering != 0 {
			return ordering
		}
	}

	if rhsNext >= len(rhsParts) {
		return 0
	} else {
		return -1
	}
}
