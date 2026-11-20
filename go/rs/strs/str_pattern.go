// Port of core 1.99.1 src/str/mod.rs and alloc 1.99.1 src/str.rs: the methods
// that take a pattern.

package strs

import (
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Each method below exists once for each kind of pattern Rust accepts:
//
//	strs.Split(s, ", ")                 s.split(", ")
//	strs.SplitChar(s, ',')              s.split(',')
//	strs.SplitChars(s, []rune{',', ';'})  s.split([',', ';']) and s.split(&[',', ';'][..])
//	strs.SplitFn(s, strs.IsWhitespace)  s.split(char::is_whitespace) and s.split(|c| ..)
//
// The searching is Rust's own (pattern.go), so the corners agree: an empty
// `&str` pattern matches between every two chars and at both ends, a reverse
// search finds the matches a reverse search finds ("aaa".rsplit("aa") is not
// "aaa".split("aa") backwards), and a predicate is called for the chars in the
// order Rust calls it. Where Rust returns an iterator, these return every
// item it yields.
//
// TrimMatches has no `&str` form because Rust has none: a `&str` cannot be
// searched from both ends at once.

func findWith(m searcher) rs.Option[int] {
	i, _, ok := m.nextMatch()
	return rs.OptionOf(i, ok)
}

func rfindWith(m searcher) rs.Option[int] {
	i, _, ok := m.nextMatchBack()
	return rs.OptionOf(i, ok)
}

func splitOnceWith(s string, m searcher) rs.Option[rs.Tuple2[string, string]] {
	start, end, ok := m.nextMatch()
	if !ok {
		return rs.None[rs.Tuple2[string, string]]()
	}
	return rs.Some(rs.T2(s[:start], s[end:]))
}

func rsplitOnceWith(s string, m searcher) rs.Option[rs.Tuple2[string, string]] {
	start, end, ok := m.nextMatchBack()
	if !ok {
		return rs.None[rs.Tuple2[string, string]]()
	}
	return rs.Some(rs.T2(s[:start], s[end:]))
}

// isPrefixWith is the default Pattern::is_prefix_of.
func isPrefixWith(m searcher) bool {
	step := m.next()
	return step.kind == stepMatch && step.a == 0
}

// isSuffixWith is the default Pattern::is_suffix_of.
func isSuffixWith(s string, m searcher) bool {
	step := m.nextBack()
	return step.kind == stepMatch && step.b == len(s)
}

// stripPrefixWith is the default Pattern::strip_prefix_of.
func stripPrefixWith(s string, m searcher) rs.Option[string] {
	if step := m.next(); step.kind == stepMatch {
		return rs.Some(s[step.b:])
	}
	return rs.None[string]()
}

// stripSuffixWith is the default Pattern::strip_suffix_of.
func stripSuffixWith(s string, m searcher) rs.Option[string] {
	if step := m.nextBack(); step.kind == stepMatch {
		return rs.Some(s[:step.a])
	}
	return rs.None[string]()
}

// stripPrefixStr is `<&str as Pattern>::strip_prefix_of`.
func stripPrefixStr(s, prefix string) rs.Option[string] {
	if strings.HasPrefix(s, prefix) {
		return rs.Some(s[len(prefix):])
	}
	return rs.None[string]()
}

// stripSuffixStr is `<&str as Pattern>::strip_suffix_of`.
func stripSuffixStr(s, suffix string) rs.Option[string] {
	if strings.HasSuffix(s, suffix) {
		return rs.Some(s[:len(s)-len(suffix)])
	}
	return rs.None[string]()
}

// Contains is str::contains with a `&str` pattern: whether the pattern matches
// anywhere in s.
func Contains(s string, pat string) bool {
	return strIsContainedIn(pat, s)
}

// ContainsChar is str::contains with a `char` pattern: whether the pattern
// matches anywhere in s.
func ContainsChar(s string, pat rune) bool {
	return charIsContainedIn(pat, s)
}

// ContainsChars is str::contains with a `[char; N]` or `&[char]` pattern,
// which matches any one of the chars: whether the pattern matches anywhere in
// s.
func ContainsChars(s string, pat []rune) bool {
	_, _, found := newCharsSearcher(s, pat).nextMatch()
	return found
}

// ContainsFn is str::contains with a predicate pattern (`FnMut(char) ->
// bool`), which matches a char it accepts: whether the pattern matches
// anywhere in s.
func ContainsFn(s string, pat func(rune) bool) bool {
	_, _, found := newFnSearcher(s, pat).nextMatch()
	return found
}

// StartsWith is str::starts_with with a `&str` pattern: whether the pattern
// matches at the start of s.
func StartsWith(s string, pat string) bool {
	return strings.HasPrefix(s, pat)
}

// StartsWithChar is str::starts_with with a `char` pattern: whether the
// pattern matches at the start of s.
func StartsWithChar(s string, pat rune) bool {
	return strings.HasPrefix(s, CharEncodeUtf8(pat))
}

// StartsWithChars is str::starts_with with a `[char; N]` or `&[char]` pattern,
// which matches any one of the chars: whether the pattern matches at the start
// of s.
func StartsWithChars(s string, pat []rune) bool {
	return isPrefixWith(newCharsSearcher(s, pat))
}

// StartsWithFn is str::starts_with with a predicate pattern (`FnMut(char) ->
// bool`), which matches a char it accepts: whether the pattern matches at the
// start of s.
func StartsWithFn(s string, pat func(rune) bool) bool {
	return isPrefixWith(newFnSearcher(s, pat))
}

// EndsWith is str::ends_with with a `&str` pattern: whether the pattern
// matches at the end of s.
func EndsWith(s string, pat string) bool {
	return strings.HasSuffix(s, pat)
}

// EndsWithChar is str::ends_with with a `char` pattern: whether the pattern
// matches at the end of s.
func EndsWithChar(s string, pat rune) bool {
	return strings.HasSuffix(s, CharEncodeUtf8(pat))
}

// EndsWithChars is str::ends_with with a `[char; N]` or `&[char]` pattern,
// which matches any one of the chars: whether the pattern matches at the end
// of s.
func EndsWithChars(s string, pat []rune) bool {
	return isSuffixWith(s, newCharsSearcher(s, pat))
}

// EndsWithFn is str::ends_with with a predicate pattern (`FnMut(char) ->
// bool`), which matches a char it accepts: whether the pattern matches at the
// end of s.
func EndsWithFn(s string, pat func(rune) bool) bool {
	return isSuffixWith(s, newFnSearcher(s, pat))
}

// Find is str::find with a `&str` pattern: the byte index of the first match,
// or None.
func Find(s string, pat string) rs.Option[int] {
	return findWith(newStrSearcher(s, pat))
}

// FindChar is str::find with a `char` pattern: the byte index of the first
// match, or None.
func FindChar(s string, pat rune) rs.Option[int] {
	return findWith(newCharSearcher(s, pat))
}

// FindChars is str::find with a `[char; N]` or `&[char]` pattern, which
// matches any one of the chars: the byte index of the first match, or None.
func FindChars(s string, pat []rune) rs.Option[int] {
	return findWith(newCharsSearcher(s, pat))
}

// FindFn is str::find with a predicate pattern (`FnMut(char) -> bool`), which
// matches a char it accepts: the byte index of the first match, or None.
func FindFn(s string, pat func(rune) bool) rs.Option[int] {
	return findWith(newFnSearcher(s, pat))
}

// Rfind is str::rfind with a `&str` pattern: the byte index of the last match,
// or None.
func Rfind(s string, pat string) rs.Option[int] {
	return rfindWith(newStrSearcher(s, pat))
}

// RfindChar is str::rfind with a `char` pattern: the byte index of the last
// match, or None.
func RfindChar(s string, pat rune) rs.Option[int] {
	return rfindWith(newCharSearcher(s, pat))
}

// RfindChars is str::rfind with a `[char; N]` or `&[char]` pattern, which
// matches any one of the chars: the byte index of the last match, or None.
func RfindChars(s string, pat []rune) rs.Option[int] {
	return rfindWith(newCharsSearcher(s, pat))
}

// RfindFn is str::rfind with a predicate pattern (`FnMut(char) -> bool`),
// which matches a char it accepts: the byte index of the last match, or None.
func RfindFn(s string, pat func(rune) bool) rs.Option[int] {
	return rfindWith(newFnSearcher(s, pat))
}

// Split is str::split with a `&str` pattern: the parts of s between matches.
// There is always at least one part, and a match at either end leaves an empty
// part there.
func Split(s string, pat string) []string {
	return splitWith(s, newStrSearcher(s, pat))
}

// SplitChar is str::split with a `char` pattern: the parts of s between
// matches. There is always at least one part, and a match at either end leaves
// an empty part there.
func SplitChar(s string, pat rune) []string {
	return splitWith(s, newCharSearcher(s, pat))
}

// SplitChars is str::split with a `[char; N]` or `&[char]` pattern, which
// matches any one of the chars: the parts of s between matches. There is
// always at least one part, and a match at either end leaves an empty part
// there.
func SplitChars(s string, pat []rune) []string {
	return splitWith(s, newCharsSearcher(s, pat))
}

// SplitFn is str::split with a predicate pattern (`FnMut(char) -> bool`),
// which matches a char it accepts: the parts of s between matches. There is
// always at least one part, and a match at either end leaves an empty part
// there.
func SplitFn(s string, pat func(rune) bool) []string {
	return splitWith(s, newFnSearcher(s, pat))
}

// SplitInclusive is str::split_inclusive with a `&str` pattern: the parts of
// s, each ending with the match that ended it. An empty s has no parts, and a
// match at the end leaves no empty part.
func SplitInclusive(s string, pat string) []string {
	return splitInclusiveWith(s, newStrSearcher(s, pat))
}

// SplitInclusiveChar is str::split_inclusive with a `char` pattern: the parts
// of s, each ending with the match that ended it. An empty s has no parts, and
// a match at the end leaves no empty part.
func SplitInclusiveChar(s string, pat rune) []string {
	return splitInclusiveWith(s, newCharSearcher(s, pat))
}

// SplitInclusiveChars is str::split_inclusive with a `[char; N]` or `&[char]`
// pattern, which matches any one of the chars: the parts of s, each ending
// with the match that ended it. An empty s has no parts, and a match at the
// end leaves no empty part.
func SplitInclusiveChars(s string, pat []rune) []string {
	return splitInclusiveWith(s, newCharsSearcher(s, pat))
}

// SplitInclusiveFn is str::split_inclusive with a predicate pattern
// (`FnMut(char) -> bool`), which matches a char it accepts: the parts of s,
// each ending with the match that ended it. An empty s has no parts, and a
// match at the end leaves no empty part.
func SplitInclusiveFn(s string, pat func(rune) bool) []string {
	return splitInclusiveWith(s, newFnSearcher(s, pat))
}

// Rsplit is str::rsplit with a `&str` pattern: the parts of s between matches,
// searched and yielded from the end.
func Rsplit(s string, pat string) []string {
	return rsplitWith(s, newStrSearcher(s, pat))
}

// RsplitChar is str::rsplit with a `char` pattern: the parts of s between
// matches, searched and yielded from the end.
func RsplitChar(s string, pat rune) []string {
	return rsplitWith(s, newCharSearcher(s, pat))
}

// RsplitChars is str::rsplit with a `[char; N]` or `&[char]` pattern, which
// matches any one of the chars: the parts of s between matches, searched and
// yielded from the end.
func RsplitChars(s string, pat []rune) []string {
	return rsplitWith(s, newCharsSearcher(s, pat))
}

// RsplitFn is str::rsplit with a predicate pattern (`FnMut(char) -> bool`),
// which matches a char it accepts: the parts of s between matches, searched
// and yielded from the end.
func RsplitFn(s string, pat func(rune) bool) []string {
	return rsplitWith(s, newFnSearcher(s, pat))
}

// SplitTerminator is str::split_terminator with a `&str` pattern: like Split,
// but a match at the end of s ends the last part instead of leaving an empty
// one after it.
func SplitTerminator(s string, pat string) []string {
	return splitTerminatorWith(s, newStrSearcher(s, pat))
}

// SplitTerminatorChar is str::split_terminator with a `char` pattern: like
// Split, but a match at the end of s ends the last part instead of leaving an
// empty one after it.
func SplitTerminatorChar(s string, pat rune) []string {
	return splitTerminatorWith(s, newCharSearcher(s, pat))
}

// SplitTerminatorChars is str::split_terminator with a `[char; N]` or
// `&[char]` pattern, which matches any one of the chars: like Split, but a
// match at the end of s ends the last part instead of leaving an empty one
// after it.
func SplitTerminatorChars(s string, pat []rune) []string {
	return splitTerminatorWith(s, newCharsSearcher(s, pat))
}

// SplitTerminatorFn is str::split_terminator with a predicate pattern
// (`FnMut(char) -> bool`), which matches a char it accepts: like Split, but a
// match at the end of s ends the last part instead of leaving an empty one
// after it.
func SplitTerminatorFn(s string, pat func(rune) bool) []string {
	return splitTerminatorWith(s, newFnSearcher(s, pat))
}

// RsplitTerminator is str::rsplit_terminator with a `&str` pattern: like
// SplitTerminator, searched and yielded from the end.
func RsplitTerminator(s string, pat string) []string {
	return rsplitTerminatorWith(s, newStrSearcher(s, pat))
}

// RsplitTerminatorChar is str::rsplit_terminator with a `char` pattern: like
// SplitTerminator, searched and yielded from the end.
func RsplitTerminatorChar(s string, pat rune) []string {
	return rsplitTerminatorWith(s, newCharSearcher(s, pat))
}

// RsplitTerminatorChars is str::rsplit_terminator with a `[char; N]` or
// `&[char]` pattern, which matches any one of the chars: like SplitTerminator,
// searched and yielded from the end.
func RsplitTerminatorChars(s string, pat []rune) []string {
	return rsplitTerminatorWith(s, newCharsSearcher(s, pat))
}

// RsplitTerminatorFn is str::rsplit_terminator with a predicate pattern
// (`FnMut(char) -> bool`), which matches a char it accepts: like
// SplitTerminator, searched and yielded from the end.
func RsplitTerminatorFn(s string, pat func(rune) bool) []string {
	return rsplitTerminatorWith(s, newFnSearcher(s, pat))
}

// Splitn is str::splitn with a `&str` pattern: at most n parts; the last one
// is the rest of s, matches included.
func Splitn(s string, n int, pat string) []string {
	return splitnWith(s, n, newStrSearcher(s, pat))
}

// SplitnChar is str::splitn with a `char` pattern: at most n parts; the last
// one is the rest of s, matches included.
func SplitnChar(s string, n int, pat rune) []string {
	return splitnWith(s, n, newCharSearcher(s, pat))
}

// SplitnChars is str::splitn with a `[char; N]` or `&[char]` pattern, which
// matches any one of the chars: at most n parts; the last one is the rest of
// s, matches included.
func SplitnChars(s string, n int, pat []rune) []string {
	return splitnWith(s, n, newCharsSearcher(s, pat))
}

// SplitnFn is str::splitn with a predicate pattern (`FnMut(char) -> bool`),
// which matches a char it accepts: at most n parts; the last one is the rest
// of s, matches included.
func SplitnFn(s string, n int, pat func(rune) bool) []string {
	return splitnWith(s, n, newFnSearcher(s, pat))
}

// Rsplitn is str::rsplitn with a `&str` pattern: at most n parts, searched and
// yielded from the end; the last one is the rest of s.
func Rsplitn(s string, n int, pat string) []string {
	return rsplitnWith(s, n, newStrSearcher(s, pat))
}

// RsplitnChar is str::rsplitn with a `char` pattern: at most n parts, searched
// and yielded from the end; the last one is the rest of s.
func RsplitnChar(s string, n int, pat rune) []string {
	return rsplitnWith(s, n, newCharSearcher(s, pat))
}

// RsplitnChars is str::rsplitn with a `[char; N]` or `&[char]` pattern, which
// matches any one of the chars: at most n parts, searched and yielded from the
// end; the last one is the rest of s.
func RsplitnChars(s string, n int, pat []rune) []string {
	return rsplitnWith(s, n, newCharsSearcher(s, pat))
}

// RsplitnFn is str::rsplitn with a predicate pattern (`FnMut(char) -> bool`),
// which matches a char it accepts: at most n parts, searched and yielded from
// the end; the last one is the rest of s.
func RsplitnFn(s string, n int, pat func(rune) bool) []string {
	return rsplitnWith(s, n, newFnSearcher(s, pat))
}

// SplitOnce is str::split_once with a `&str` pattern: s before and after the
// first match, or None when there is no match.
func SplitOnce(s string, pat string) rs.Option[rs.Tuple2[string, string]] {
	return splitOnceWith(s, newStrSearcher(s, pat))
}

// SplitOnceChar is str::split_once with a `char` pattern: s before and after
// the first match, or None when there is no match.
func SplitOnceChar(s string, pat rune) rs.Option[rs.Tuple2[string, string]] {
	return splitOnceWith(s, newCharSearcher(s, pat))
}

// SplitOnceChars is str::split_once with a `[char; N]` or `&[char]` pattern,
// which matches any one of the chars: s before and after the first match, or
// None when there is no match.
func SplitOnceChars(s string, pat []rune) rs.Option[rs.Tuple2[string, string]] {
	return splitOnceWith(s, newCharsSearcher(s, pat))
}

// SplitOnceFn is str::split_once with a predicate pattern (`FnMut(char) ->
// bool`), which matches a char it accepts: s before and after the first match,
// or None when there is no match.
func SplitOnceFn(s string, pat func(rune) bool) rs.Option[rs.Tuple2[string, string]] {
	return splitOnceWith(s, newFnSearcher(s, pat))
}

// RsplitOnce is str::rsplit_once with a `&str` pattern: s before and after the
// last match, or None when there is no match.
func RsplitOnce(s string, pat string) rs.Option[rs.Tuple2[string, string]] {
	return rsplitOnceWith(s, newStrSearcher(s, pat))
}

// RsplitOnceChar is str::rsplit_once with a `char` pattern: s before and after
// the last match, or None when there is no match.
func RsplitOnceChar(s string, pat rune) rs.Option[rs.Tuple2[string, string]] {
	return rsplitOnceWith(s, newCharSearcher(s, pat))
}

// RsplitOnceChars is str::rsplit_once with a `[char; N]` or `&[char]` pattern,
// which matches any one of the chars: s before and after the last match, or
// None when there is no match.
func RsplitOnceChars(s string, pat []rune) rs.Option[rs.Tuple2[string, string]] {
	return rsplitOnceWith(s, newCharsSearcher(s, pat))
}

// RsplitOnceFn is str::rsplit_once with a predicate pattern (`FnMut(char) ->
// bool`), which matches a char it accepts: s before and after the last match,
// or None when there is no match.
func RsplitOnceFn(s string, pat func(rune) bool) rs.Option[rs.Tuple2[string, string]] {
	return rsplitOnceWith(s, newFnSearcher(s, pat))
}

// Matches is str::matches with a `&str` pattern: every match, in order;
// matches do not overlap.
func Matches(s string, pat string) []string {
	return matchesWith(s, newStrSearcher(s, pat))
}

// MatchesChar is str::matches with a `char` pattern: every match, in order;
// matches do not overlap.
func MatchesChar(s string, pat rune) []string {
	return matchesWith(s, newCharSearcher(s, pat))
}

// MatchesChars is str::matches with a `[char; N]` or `&[char]` pattern, which
// matches any one of the chars: every match, in order; matches do not overlap.
func MatchesChars(s string, pat []rune) []string {
	return matchesWith(s, newCharsSearcher(s, pat))
}

// MatchesFn is str::matches with a predicate pattern (`FnMut(char) -> bool`),
// which matches a char it accepts: every match, in order; matches do not
// overlap.
func MatchesFn(s string, pat func(rune) bool) []string {
	return matchesWith(s, newFnSearcher(s, pat))
}

// Rmatches is str::rmatches with a `&str` pattern: every match, searched and
// yielded from the end.
func Rmatches(s string, pat string) []string {
	return rmatchesWith(s, newStrSearcher(s, pat))
}

// RmatchesChar is str::rmatches with a `char` pattern: every match, searched
// and yielded from the end.
func RmatchesChar(s string, pat rune) []string {
	return rmatchesWith(s, newCharSearcher(s, pat))
}

// RmatchesChars is str::rmatches with a `[char; N]` or `&[char]` pattern,
// which matches any one of the chars: every match, searched and yielded from
// the end.
func RmatchesChars(s string, pat []rune) []string {
	return rmatchesWith(s, newCharsSearcher(s, pat))
}

// RmatchesFn is str::rmatches with a predicate pattern (`FnMut(char) ->
// bool`), which matches a char it accepts: every match, searched and yielded
// from the end.
func RmatchesFn(s string, pat func(rune) bool) []string {
	return rmatchesWith(s, newFnSearcher(s, pat))
}

// MatchIndices is str::match_indices with a `&str` pattern: every match with
// the byte index it starts at.
func MatchIndices(s string, pat string) []rs.Tuple2[int, string] {
	return matchIndicesWith(s, newStrSearcher(s, pat))
}

// MatchIndicesChar is str::match_indices with a `char` pattern: every match
// with the byte index it starts at.
func MatchIndicesChar(s string, pat rune) []rs.Tuple2[int, string] {
	return matchIndicesWith(s, newCharSearcher(s, pat))
}

// MatchIndicesChars is str::match_indices with a `[char; N]` or `&[char]`
// pattern, which matches any one of the chars: every match with the byte index
// it starts at.
func MatchIndicesChars(s string, pat []rune) []rs.Tuple2[int, string] {
	return matchIndicesWith(s, newCharsSearcher(s, pat))
}

// MatchIndicesFn is str::match_indices with a predicate pattern (`FnMut(char)
// -> bool`), which matches a char it accepts: every match with the byte index
// it starts at.
func MatchIndicesFn(s string, pat func(rune) bool) []rs.Tuple2[int, string] {
	return matchIndicesWith(s, newFnSearcher(s, pat))
}

// RmatchIndices is str::rmatch_indices with a `&str` pattern: every match with
// the byte index it starts at, searched and yielded from the end.
func RmatchIndices(s string, pat string) []rs.Tuple2[int, string] {
	return rmatchIndicesWith(s, newStrSearcher(s, pat))
}

// RmatchIndicesChar is str::rmatch_indices with a `char` pattern: every match
// with the byte index it starts at, searched and yielded from the end.
func RmatchIndicesChar(s string, pat rune) []rs.Tuple2[int, string] {
	return rmatchIndicesWith(s, newCharSearcher(s, pat))
}

// RmatchIndicesChars is str::rmatch_indices with a `[char; N]` or `&[char]`
// pattern, which matches any one of the chars: every match with the byte index
// it starts at, searched and yielded from the end.
func RmatchIndicesChars(s string, pat []rune) []rs.Tuple2[int, string] {
	return rmatchIndicesWith(s, newCharsSearcher(s, pat))
}

// RmatchIndicesFn is str::rmatch_indices with a predicate pattern
// (`FnMut(char) -> bool`), which matches a char it accepts: every match with
// the byte index it starts at, searched and yielded from the end.
func RmatchIndicesFn(s string, pat func(rune) bool) []rs.Tuple2[int, string] {
	return rmatchIndicesWith(s, newFnSearcher(s, pat))
}

// TrimMatchesChar is str::trim_matches with a `char` pattern: s without the
// matches at its start and at its end.
func TrimMatchesChar(s string, pat rune) string {
	return trimMatchesWith(s, newCharSearcher(s, pat))
}

// TrimMatchesChars is str::trim_matches with a `[char; N]` or `&[char]`
// pattern, which matches any one of the chars: s without the matches at its
// start and at its end.
func TrimMatchesChars(s string, pat []rune) string {
	return trimMatchesWith(s, newCharsSearcher(s, pat))
}

// TrimMatchesFn is str::trim_matches with a predicate pattern (`FnMut(char) ->
// bool`), which matches a char it accepts: s without the matches at its start
// and at its end.
func TrimMatchesFn(s string, pat func(rune) bool) string {
	return trimMatchesWith(s, newFnSearcher(s, pat))
}

// TrimStartMatches is str::trim_start_matches with a `&str` pattern: s without
// the matches at its start, however many times the pattern repeats there.
func TrimStartMatches(s string, pat string) string {
	return trimStartMatchesWith(s, newStrSearcher(s, pat))
}

// TrimStartMatchesChar is str::trim_start_matches with a `char` pattern: s
// without the matches at its start, however many times the pattern repeats
// there.
func TrimStartMatchesChar(s string, pat rune) string {
	return trimStartMatchesWith(s, newCharSearcher(s, pat))
}

// TrimStartMatchesChars is str::trim_start_matches with a `[char; N]` or
// `&[char]` pattern, which matches any one of the chars: s without the matches
// at its start, however many times the pattern repeats there.
func TrimStartMatchesChars(s string, pat []rune) string {
	return trimStartMatchesWith(s, newCharsSearcher(s, pat))
}

// TrimStartMatchesFn is str::trim_start_matches with a predicate pattern
// (`FnMut(char) -> bool`), which matches a char it accepts: s without the
// matches at its start, however many times the pattern repeats there.
func TrimStartMatchesFn(s string, pat func(rune) bool) string {
	return trimStartMatchesWith(s, newFnSearcher(s, pat))
}

// TrimEndMatches is str::trim_end_matches with a `&str` pattern: s without the
// matches at its end, however many times the pattern repeats there.
func TrimEndMatches(s string, pat string) string {
	return trimEndMatchesWith(s, newStrSearcher(s, pat))
}

// TrimEndMatchesChar is str::trim_end_matches with a `char` pattern: s without
// the matches at its end, however many times the pattern repeats there.
func TrimEndMatchesChar(s string, pat rune) string {
	return trimEndMatchesWith(s, newCharSearcher(s, pat))
}

// TrimEndMatchesChars is str::trim_end_matches with a `[char; N]` or `&[char]`
// pattern, which matches any one of the chars: s without the matches at its
// end, however many times the pattern repeats there.
func TrimEndMatchesChars(s string, pat []rune) string {
	return trimEndMatchesWith(s, newCharsSearcher(s, pat))
}

// TrimEndMatchesFn is str::trim_end_matches with a predicate pattern
// (`FnMut(char) -> bool`), which matches a char it accepts: s without the
// matches at its end, however many times the pattern repeats there.
func TrimEndMatchesFn(s string, pat func(rune) bool) string {
	return trimEndMatchesWith(s, newFnSearcher(s, pat))
}

// StripPrefix is str::strip_prefix with a `&str` pattern: s without one match
// at its start, or None when s does not start with the pattern.
func StripPrefix(s string, pat string) rs.Option[string] {
	return stripPrefixStr(s, pat)
}

// StripPrefixChar is str::strip_prefix with a `char` pattern: s without one
// match at its start, or None when s does not start with the pattern.
func StripPrefixChar(s string, pat rune) rs.Option[string] {
	return stripPrefixStr(s, CharEncodeUtf8(pat))
}

// StripPrefixChars is str::strip_prefix with a `[char; N]` or `&[char]`
// pattern, which matches any one of the chars: s without one match at its
// start, or None when s does not start with the pattern.
func StripPrefixChars(s string, pat []rune) rs.Option[string] {
	return stripPrefixWith(s, newCharsSearcher(s, pat))
}

// StripPrefixFn is str::strip_prefix with a predicate pattern (`FnMut(char) ->
// bool`), which matches a char it accepts: s without one match at its start,
// or None when s does not start with the pattern.
func StripPrefixFn(s string, pat func(rune) bool) rs.Option[string] {
	return stripPrefixWith(s, newFnSearcher(s, pat))
}

// StripSuffix is str::strip_suffix with a `&str` pattern: s without one match
// at its end, or None when s does not end with the pattern.
func StripSuffix(s string, pat string) rs.Option[string] {
	return stripSuffixStr(s, pat)
}

// StripSuffixChar is str::strip_suffix with a `char` pattern: s without one
// match at its end, or None when s does not end with the pattern.
func StripSuffixChar(s string, pat rune) rs.Option[string] {
	return stripSuffixStr(s, CharEncodeUtf8(pat))
}

// StripSuffixChars is str::strip_suffix with a `[char; N]` or `&[char]`
// pattern, which matches any one of the chars: s without one match at its end,
// or None when s does not end with the pattern.
func StripSuffixChars(s string, pat []rune) rs.Option[string] {
	return stripSuffixWith(s, newCharsSearcher(s, pat))
}

// StripSuffixFn is str::strip_suffix with a predicate pattern (`FnMut(char) ->
// bool`), which matches a char it accepts: s without one match at its end, or
// None when s does not end with the pattern.
func StripSuffixFn(s string, pat func(rune) bool) rs.Option[string] {
	return stripSuffixWith(s, newFnSearcher(s, pat))
}

// Replace is str::replace with a `&str` pattern: s with every match replaced
// by to.
func Replace(s string, pat string, to string) string {
	return replaceWith(s, newStrSearcher(s, pat), to)
}

// ReplaceChar is str::replace with a `char` pattern: s with every match
// replaced by to.
func ReplaceChar(s string, pat rune, to string) string {
	return replaceWith(s, newCharSearcher(s, pat), to)
}

// ReplaceChars is str::replace with a `[char; N]` or `&[char]` pattern, which
// matches any one of the chars: s with every match replaced by to.
func ReplaceChars(s string, pat []rune, to string) string {
	return replaceWith(s, newCharsSearcher(s, pat), to)
}

// ReplaceFn is str::replace with a predicate pattern (`FnMut(char) -> bool`),
// which matches a char it accepts: s with every match replaced by to.
func ReplaceFn(s string, pat func(rune) bool, to string) string {
	return replaceWith(s, newFnSearcher(s, pat), to)
}

// Replacen is str::replacen with a `&str` pattern: s with the first count
// matches replaced by to.
func Replacen(s string, pat string, to string, count int) string {
	return replacenWith(s, newStrSearcher(s, pat), to, count)
}

// ReplacenChar is str::replacen with a `char` pattern: s with the first count
// matches replaced by to.
func ReplacenChar(s string, pat rune, to string, count int) string {
	return replacenWith(s, newCharSearcher(s, pat), to, count)
}

// ReplacenChars is str::replacen with a `[char; N]` or `&[char]` pattern,
// which matches any one of the chars: s with the first count matches replaced
// by to.
func ReplacenChars(s string, pat []rune, to string, count int) string {
	return replacenWith(s, newCharsSearcher(s, pat), to, count)
}

// ReplacenFn is str::replacen with a predicate pattern (`FnMut(char) ->
// bool`), which matches a char it accepts: s with the first count matches
// replaced by to.
func ReplacenFn(s string, pat func(rune) bool, to string, count int) string {
	return replacenWith(s, newFnSearcher(s, pat), to, count)
}
