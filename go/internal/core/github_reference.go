// Port of mj-core/src/github_reference.rs.

// GitHub references shared by transcript readers.

package core

import (
	"regexp"
	"strconv"
	"unicode"
	"unicode/utf8"
)

type GithubReference struct {
	Owner  *string
	Repo   *string
	Number uint64
	Text   string
	// Byte offset of `Text` in the parsed string.
	Start int
}

var githubReferencePattern = regexp.MustCompile(`(?i)https://github\.com/([a-z0-9_-]+)/([a-z0-9_.-]+)/(?:issues|pull)/([0-9]+)|(?:([a-z0-9_-]+)/([a-z0-9_.-]+))?#([0-9]+)`)

func GithubReferences(text string) []GithubReference {
	var references []GithubReference
	for _, capture := range githubReferencePattern.FindAllStringSubmatchIndex(text, -1) {
		start, end := capture[0], capture[1]
		boundary := func(c rune) bool {
			return rustIsAlphanumeric(c) || c == '_' || c == '/' || c == '#' || c == '-'
		}
		if before, size := utf8.DecodeLastRuneInString(text[:start]); size > 0 && boundary(before) {
			continue
		}
		if after, size := utf8.DecodeRuneInString(text[end:]); size > 0 && (rustIsAlphanumeric(after) || after == '_') {
			continue
		}
		group := func(index int) *string {
			if capture[2*index] < 0 {
				return nil
			}
			value := text[capture[2*index]:capture[2*index+1]]
			return &value
		}
		owner, repo, number := 4, 5, 6
		if capture[6] >= 0 {
			owner, repo, number = 1, 2, 3
		}
		digits := group(number)
		if digits == nil {
			continue
		}
		parsed, err := strconv.ParseUint(*digits, 10, 64)
		if err != nil {
			continue
		}
		references = append(references, GithubReference{
			Owner:  group(owner),
			Repo:   group(repo),
			Number: parsed,
			Text:   text[start:end],
			Start:  start,
		})
	}
	return references
}

// rustIsAlphanumeric is Rust's `char::is_alphanumeric`: the Unicode
// Alphabetic or Numeric property.
func rustIsAlphanumeric(c rune) bool {
	return unicode.In(c, unicode.L, unicode.Nl, unicode.Other_Alphabetic, unicode.Nd, unicode.No)
}
