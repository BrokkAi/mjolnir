// Port of mj-tui/src/setup/search.rs.

// A flat index over every settings page and value.
//
// Settings are stored as a tree and browsed one page at a time, so reaching a
// value means already knowing which section holds it. The index makes every
// row addressable by what it is called, what it does, and what it is set to,
// so the dialog can jump straight to one.

package tui

import (
	"slices"
	"strings"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
)

// SearchEntry is one row the dialog can navigate to, addressed by the same
// display path that browsing to it would have produced.
type SearchEntry struct {
	// path is the display path, which keeps the synthetic `interface`
	// section that client.Pointer strips before it touches the draft.
	path  []string
	label string
	// trail is the pages above this row, such as `Runtimes › podman`.
	trail string
	// value is what the row shows on its own page, so a search often
	// answers the question without opening anything.
	value string
	// haystack is everything matched against, lowercased once at build
	// time.
	haystack string
}

// setupSearchIndex is every page and value in draft, in the order the pages
// list them (Rust's search::index).
func setupSearchIndex(draft any) []SearchEntry {
	var entries []SearchEntry
	setupSearchWalk(draft, []string{}, "", &entries)
	return entries
}

// setupSearchWalk is Rust's search::walk.
func setupSearchWalk(draft any, path []string, trail string, entries *[]SearchEntry) {
	panic("unported: mj-tui/src/setup/search.rs walk")
}

// setupSearchPageSummary is what a page row holds: a first-page section's
// own summary, or the count the page row shows everywhere
// (`contents_summary`).
func setupSearchPageSummary(path []string, value, draft any) (string, bool) {
	if !setupValueIsObject(value) && !setupValueIsArray(value) {
		return "", false
	}
	// A first-page section says what it is set to, exactly as the page does.
	if len(path) == 1 {
		if summary, ok := setupSectionSummary(path[0], draft); ok {
			return summary, true
		}
	}
	if summary, ok := machineBuildCacheSummary(path, value, nil); ok {
		return summary, true
	}
	if summary, ok := client.ContentsSummary(path, value); ok {
		return summary + "  ›", true
	}
	return "", false
}

// setupSearchMatches is the entries matching query, as indices into entries.
//
// An empty query matches everything, which makes opening the search a flat
// view of every setting. The ranking is the command palette's rule: a label
// the query starts, else anything the query appears in.
func setupSearchMatches(entries []SearchEntry, query string) []int {
	query = strings.ToLower(strings.TrimSpace(query))
	all := make([]int, 0, len(entries))
	var prefix, contains []int
	for index, entry := range entries {
		all = append(all, index)
		if strings.HasPrefix(strings.ToLower(entry.label), query) ||
			slices.ContainsFunc(client.SearchAliases(entry.path), func(alias string) bool {
				return strings.HasPrefix(alias, query)
			}) {
			prefix = append(prefix, index)
		}
		if strings.Contains(entry.haystack, query) {
			contains = append(contains, index)
		}
	}
	switch {
	case query == "":
		return all
	case len(prefix) > 0:
		return prefix
	case contains == nil:
		return []int{}
	default:
		return contains
	}
}

// setupSearchRow is one result row: the name, the pages it lives under, and
// its current value.
func setupSearchRow(entry *SearchEntry, width int) chat.Line {
	width = max(width, 24)
	labelWidth := min(max(width/3, 12), 34)
	trailWidth := min(max(width/3, 10), 32)
	valueWidth := max(width-(labelWidth+trailWidth+4), 0)
	return chat.LineFrom(
		chat.RawSpan(setupSearchPad(entry.label, labelWidth)),
		chat.RawSpan("  "),
		chat.StyledSpan(setupSearchPad(entry.trail, trailWidth), chat.Muted()),
		chat.RawSpan("  "),
		chat.StyledSpan(setupSearchClip(entry.value, valueWidth), chat.Muted()),
	)
}

func setupSearchClip(text string, width int) string {
	if utf8.RuneCountInString(text) <= width {
		return text
	}
	runes := []rune(text)
	return string(runes[:max(width-1, 0)]) + "…"
}

func setupSearchPad(text string, width int) string {
	text = setupSearchClip(text, width)
	padding := max(width-utf8.RuneCountInString(text), 0)
	return text + strings.Repeat(" ", padding)
}
