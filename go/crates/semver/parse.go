// Port of semver 1.0.28 src/parse.rs

package semver

import (
	"math"
	"strings"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Error parsing a SemVer version or version requirement.
//
// # Example
//
//	_, err := semver.VersionParse("1.q.r")
//
//	// "unexpected character 'q' while parsing minor version number"
//	rs.Eprintln("{}", err)
type Error struct {
	kind errorErrorKind
}

// VersionFromStr is `impl FromStr for Version`. The error is a *Error.
func VersionFromStr(text string) (Version, error) {
	if text == "" {
		return Version{}, errorNew(&errorErrorKind_Empty{})
	}

	pos := errorPosition_Major
	major, text, err := parseNumericIdentifier(text, pos)
	if err != nil {
		return Version{}, err
	}
	text, err = parseDot(text, pos)
	if err != nil {
		return Version{}, err
	}

	pos = errorPosition_Minor
	minor, text, err := parseNumericIdentifier(text, pos)
	if err != nil {
		return Version{}, err
	}
	text, err = parseDot(text, pos)
	if err != nil {
		return Version{}, err
	}

	pos = errorPosition_Patch
	patch, text, err := parseNumericIdentifier(text, pos)
	if err != nil {
		return Version{}, err
	}

	if text == "" {
		return VersionNew(major, minor, patch), nil
	}

	var pre Prerelease
	if rest, ok := strings.CutPrefix(text, "-"); ok {
		pos = errorPosition_Pre
		pre, text, err = parsePrereleaseIdentifier(rest)
		if err != nil {
			return Version{}, err
		}
		if pre.IsEmpty() {
			return Version{}, errorNew(&errorErrorKind_EmptySegment{F0: pos})
		}
	} else {
		pre = PrereleaseEmpty
	}

	var build BuildMetadata
	if rest, ok := strings.CutPrefix(text, "+"); ok {
		pos = errorPosition_Build
		build, text, err = parseBuildIdentifier(rest)
		if err != nil {
			return Version{}, err
		}
		if build.IsEmpty() {
			return Version{}, errorNew(&errorErrorKind_EmptySegment{F0: pos})
		}
	} else {
		build = BuildMetadataEmpty
	}

	if unexpected, ok := parseFirstChar(text).Get(); ok {
		return Version{}, errorNew(&errorErrorKind_UnexpectedCharAfter{F0: pos, F1: unexpected})
	}

	return Version{
		Major: major,
		Minor: minor,
		Patch: patch,
		Pre:   pre,
		Build: build,
	}, nil
}

// PrereleaseFromStr is `impl FromStr for Prerelease`. The error is a *Error.
func PrereleaseFromStr(text string) (Prerelease, error) {
	pre, rest, err := parsePrereleaseIdentifier(text)
	if err != nil {
		return Prerelease{}, err
	}
	if rest != "" {
		return Prerelease{}, errorNew(&errorErrorKind_IllegalCharacter{F0: errorPosition_Pre})
	}
	return pre, nil
}

// BuildMetadataFromStr is `impl FromStr for BuildMetadata`. The error is a
// *Error.
func BuildMetadataFromStr(text string) (BuildMetadata, error) {
	build, rest, err := parseBuildIdentifier(text)
	if err != nil {
		return BuildMetadata{}, err
	}
	if rest != "" {
		return BuildMetadata{}, errorNew(&errorErrorKind_IllegalCharacter{F0: errorPosition_Build})
	}
	return build, nil
}

func errorNew(kind errorErrorKind) *Error {
	return &Error{kind: kind}
}

// parseFirstChar is `text.chars().next()`.
func parseFirstChar(text string) rs.Option[rune] {
	if text == "" {
		return rs.None[rune]()
	}
	c, _ := utf8.DecodeRuneInString(text)
	return rs.Some(c)
}

func parseNumericIdentifier(input string, pos errorPosition) (uint64, string, error) {
	len_ := 0
	value := uint64(0)

	for len_ < len(input) {
		digit := input[len_]
		if digit < '0' || digit > '9' {
			break
		}
		if value == 0 && len_ > 0 {
			return 0, "", errorNew(&errorErrorKind_LeadingZero{F0: pos})
		}
		// value.checked_mul(10).and_then(|value| value.checked_add((digit - b'0') as u64))
		if value > math.MaxUint64/10 {
			return 0, "", errorNew(&errorErrorKind_Overflow{F0: pos})
		}
		product := value * 10
		sum := product + uint64(digit-'0')
		if sum < product {
			return 0, "", errorNew(&errorErrorKind_Overflow{F0: pos})
		}
		value = sum
		len_ += 1
	}

	if len_ > 0 {
		return value, input[len_:], nil
	} else if unexpected, ok := parseFirstChar(input[len_:]).Get(); ok {
		return 0, "", errorNew(&errorErrorKind_UnexpectedChar{F0: pos, F1: unexpected})
	} else {
		return 0, "", errorNew(&errorErrorKind_UnexpectedEnd{F0: pos})
	}
}

func parseDot(input string, pos errorPosition) (string, error) {
	if rest, ok := strings.CutPrefix(input, "."); ok {
		return rest, nil
	} else if unexpected, ok := parseFirstChar(input).Get(); ok {
		return "", errorNew(&errorErrorKind_UnexpectedCharAfter{F0: pos, F1: unexpected})
	} else {
		return "", errorNew(&errorErrorKind_UnexpectedEnd{F0: pos})
	}
}

func parsePrereleaseIdentifier(input string) (Prerelease, string, error) {
	string_, rest, err := parseIdentifier(input, errorPosition_Pre)
	if err != nil {
		return Prerelease{}, "", err
	}
	identifier := identifierIdentifierNewUnchecked(string_)
	return Prerelease{identifier: identifier}, rest, nil
}

func parseBuildIdentifier(input string) (BuildMetadata, string, error) {
	string_, rest, err := parseIdentifier(input, errorPosition_Build)
	if err != nil {
		return BuildMetadata{}, "", err
	}
	identifier := identifierIdentifierNewUnchecked(string_)
	return BuildMetadata{identifier: identifier}, rest, nil
}

func parseIdentifier(input string, pos errorPosition) (string, string, error) {
	accumulatedLen := 0
	segmentLen := 0
	segmentHasNondigit := false

	for {
		// input.as_bytes().get(accumulated_len + segment_len)
		boundary := rs.None[byte]()
		if index := accumulatedLen + segmentLen; index < len(input) {
			boundary = rs.Some(input[index])
		}
		b, some := boundary.Get()
		switch {
		case some && (('A' <= b && b <= 'Z') || ('a' <= b && b <= 'z') || b == '-'):
			segmentLen += 1
			segmentHasNondigit = true
		case some && ('0' <= b && b <= '9'):
			segmentLen += 1
		default:
			if segmentLen == 0 {
				if accumulatedLen == 0 && boundary != rs.Some(byte('.')) {
					return "", input, nil
				} else {
					return "", "", errorNew(&errorErrorKind_EmptySegment{F0: pos})
				}
			}
			if pos == errorPosition_Pre &&
				segmentLen > 1 &&
				!segmentHasNondigit &&
				strings.HasPrefix(input[accumulatedLen:], "0") {
				return "", "", errorNew(&errorErrorKind_LeadingZero{F0: pos})
			}
			accumulatedLen += segmentLen
			if boundary == rs.Some(byte('.')) {
				accumulatedLen += 1
				segmentLen = 0
				segmentHasNondigit = false
			} else {
				return input[:accumulatedLen], input[accumulatedLen:], nil
			}
		}
	}
}
