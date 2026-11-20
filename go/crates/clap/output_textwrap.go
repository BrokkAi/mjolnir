// Port of clap_builder 4.6.0 src/output/textwrap/mod.rs,
// src/output/textwrap/core.rs, src/output/textwrap/word_separators.rs and
// src/output/textwrap/wrap_algorithms.rs
//
// Fork of `textwrap` crate
//
// Benefits of forking:
// - Pull in only what we need rather than relying on the compiler to remove what we don't need
// - `LineWrapper` is able to incrementally wrap which will help with `StyledStr`

package clap

import "strings"

// featureWrapHelp is the crate's `wrap_help` feature. Mjolnir builds clap
// without it (clap_builder has no terminal_size dependency in Cargo.lock),
// so help text is never wrapped and the terminal width is never queried.
// The wrapping code is ported all the same and is what the feature would
// run.
const featureWrapHelp = false

// textwrapWrap is textwrap::wrap.
func textwrapWrap(content string, hardWidth int) string {
	if !featureWrapHelp {
		return content
	}
	wrapper := textwrapLineWrapperNew(hardWidth)
	var total []string
	for _, line := range strSplitInclusive(content, '\n') {
		wrapper.reset()
		words := textwrapFindWordsAsciiSpace(line)
		total = append(total, wrapper.wrap(words)...)
	}
	return strings.Join(total, "")
}

// textwrapDisplayWidth is textwrap::core::display_width: compute the
// display width of `text`, skipping the escape sequences.
func textwrapDisplayWidth(text string) int {
	width := 0

	controlSequence := false
	const controlTerminate = 'm'

	for _, ch := range text {
		if ch < 0x80 && (ch < 0x20 || ch == 0x7f) {
			controlSequence = true
		} else if controlSequence && ch == controlTerminate {
			controlSequence = false
			continue
		}

		if !controlSequence {
			width += textwrapChWidth(ch)
		}
	}
	return width
}

// textwrapChWidth is ch_width without the crate's `unicode` feature, which
// Mjolnir does not enable: every character is one column wide.
func textwrapChWidth(rune) int {
	return 1
}

// textwrapFindWordsAsciiSpace splits line into words separated by regions
// of ' ' characters.
//
// Examples
//
//	"Hello World!" -> ["Hello ", "World!"]
func textwrapFindWordsAsciiSpace(line string) []string {
	var words []string
	start := 0
	inWhitespace := false
	for idx, ch := range line {
		nextWhitespace := ch == ' '
		if inWhitespace && !nextWhitespace {
			words = append(words, line[start:idx])
			start = idx
		}
		inWhitespace = nextWhitespace
	}

	if start < len(line) {
		words = append(words, line[start:])
	}
	return words
}

// textwrapLineWrapper is LineWrapper.
type textwrapLineWrapper struct {
	hardWidth   int
	lineWidth   int
	indentation *string
}

func textwrapLineWrapperNew(hardWidth int) *textwrapLineWrapper {
	return &textwrapLineWrapper{
		hardWidth:   hardWidth,
		lineWidth:   0,
		indentation: nil,
	}
}

func (w *textwrapLineWrapper) reset() {
	w.lineWidth = 0
	w.indentation = nil
}

func (w *textwrapLineWrapper) wrap(words []string) []string {
	firstWord := false
	if w.indentation == nil {
		firstWord = true
		if len(words) > 0 {
			word := words[0]
			if strTrim(word) == "" {
				w.indentation = &word
			} else {
				empty := ""
				w.indentation = &empty
			}
		}
	}

	i := 0
	for i < len(words) {
		word := words[i]
		trimmed := strTrimEnd(word)
		wordWidth := textwrapDisplayWidth(trimmed)
		trimmedDelta := len(word) - len(trimmed)
		if firstWord && 0 < wordWidth {
			// Never try to wrap the first word
			firstWord = false
		} else if w.hardWidth < w.lineWidth+wordWidth {
			if 0 < i {
				prev := i - 1
				trimmed := strTrimEnd(words[prev])
				words[prev] = trimmed
			}

			w.lineWidth = 0
			words = append(words[:i], append([]string{"\n"}, words[i:]...)...)
			i++
			if w.indentation != nil {
				indentation := *w.indentation
				words = append(words[:i], append([]string{indentation}, words[i:]...)...)
				w.lineWidth += len(indentation)
				i++
			}
		}
		w.lineWidth += wordWidth + trimmedDelta

		i++
	}
	return words
}
