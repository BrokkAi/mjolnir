// Port of clap_builder 4.6.0 src/parser/features/suggestions.rs and
// strsim 0.11.1 src/lib.rs (jaro)

package clap

import "github.com/BrokkAi/mjolnir/go/rs"

// suggestionsDidYouMean finds strings from an iterable of `possible_values`
// similar to a given value `v`. Returns a Vec of all possible values that
// exceed a similarity threshold sorted by ascending similarity, most
// similar comes last.
func suggestionsDidYouMean(v string, possibleValues []string) []string {
	type candidate struct {
		confidence float64
		value      string
	}
	var candidates []candidate
	for _, pv := range possibleValues {
		// GH #4660: using `jaro` because `jaro_winkler` implementation in `strsim-rs` is wrong
		// causing strings with common prefix >=10 to be considered perfectly similar
		confidence := strsimJaro(v, pv)

		if confidence > 0.7 {
			newElem := candidate{confidence: confidence, value: pv}
			// binary_search_by with a comparison that is never Equal: the
			// position of the first candidate with a greater confidence.
			pos := len(candidates)
			for i := range candidates {
				if candidates[i].confidence > confidence {
					pos = i
					break
				}
			}
			candidates = append(candidates, candidate{})
			copy(candidates[pos+1:], candidates[pos:])
			candidates[pos] = newElem
		}
	}

	values := make([]string, len(candidates))
	for i, c := range candidates {
		values[i] = c.value
	}
	return values
}

// suggestionsFlag is the `(String, Option<String>)` did_you_mean_flag
// returns: the suggested flag, and the subcommand that has it when it is
// not this command's.
type suggestionsFlag struct {
	flag       string
	subcommand rs.Option[string]
}

// suggestionsDidYouMeanFlag returns a suffix that can be empty, or is the
// standard 'did you mean' phrase.
func suggestionsDidYouMeanFlag(arg string, remainingArgs []string, longs []string, subcommands []*Command) rs.Option[suggestionsFlag] {
	candidates := suggestionsDidYouMean(arg, longs)
	if len(candidates) > 0 {
		return rs.Some(suggestionsFlag{flag: candidates[len(candidates)-1], subcommand: rs.None[string]()})
	}

	// min_by_key keeps the first of the entries that share the least score.
	best := rs.None[suggestionsFlag]()
	bestScore := 0
	for _, subcommand := range subcommands {
		subcommand.buildSelf(false)

		var subLongs []string
		for i := range subcommand.getKeymap().keys {
			if key := subcommand.getKeymap().keys[i].key; key.kind == mkeymapKeyType_Long {
				subLongs = append(subLongs, toStringLossy(key.long))
			}
		}

		subcommandName := subcommand.GetName()

		subCandidates := suggestionsDidYouMean(arg, subLongs)
		if len(subCandidates) == 0 {
			continue
		}
		candidate := subCandidates[len(subCandidates)-1]
		score := -1
		for i, x := range remainingArgs {
			if subcommandName == x {
				score = i
				break
			}
		}
		if score < 0 {
			continue
		}
		if best.IsNone() || score < bestScore {
			best = rs.Some(suggestionsFlag{flag: candidate, subcommand: rs.Some(subcommandName)})
			bestScore = score
		}
	}
	return best
}

// strsimJaro is strsim::jaro: calculates the Jaro similarity between two
// strings. The returned value is between 0.0 and 1.0 (higher value means
// more similar).
func strsimJaro(a, b string) float64 {
	aChars := []rune(a)
	bChars := []rune(b)
	aLen := len(aChars)
	bLen := len(bChars)

	if aLen == 0 && bLen == 0 {
		return 1.0
	} else if aLen == 0 || bLen == 0 {
		return 0.0
	}

	searchRange := max(aLen, bLen) / 2
	if searchRange > 0 {
		searchRange--
	}

	aFlags := make([]bool, aLen)
	bFlags := make([]bool, bLen)

	matches := 0

	for i, aElem := range aChars {
		// prevent integer wrapping
		minBound := 0
		if i > searchRange {
			minBound = i - searchRange
		}

		maxBound := min(bLen, i+searchRange+1)

		for j := 0; j < maxBound; j++ {
			if minBound <= j && aElem == bChars[j] && !bFlags[j] {
				aFlags[i] = true
				bFlags[j] = true
				matches++
				break
			}
		}
	}

	transpositions := 0
	if matches != 0 {
		bIter := 0
		for i, aFlag := range aFlags {
			if aFlag {
				for bIter < bLen {
					j := bIter
					bIter++
					if !bFlags[j] {
						continue
					}

					if aChars[i] != bChars[j] {
						transpositions++
					}
					break
				}
			}
		}
	}
	transpositions /= 2

	if matches == 0 {
		return 0.0
	}
	return ((float64(matches) / float64(aLen)) +
		(float64(matches) / float64(bLen)) +
		(float64(matches-transpositions) / float64(matches))) /
		3.0
}
