// Port of mj-tui/src/setup/page.rs.
//
// Rust's `pub(super) use mj_client::settings::page::{...}` re-exports have no
// Go form: the setup files call client.PagePlan, client.RowLabel,
// client.Breadcrumb and the rest directly.

package tui

import (
	"slices"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
)

// rowSummary is what one row reports, which on the first page is the state
// of a whole section rather than the size of it.
func rowSummary(path []string, key string, value, draft any, automatic *string) string {
	if len(path) == 0 {
		if summary, ok := setupSectionSummary(key, draft); ok {
			return summary
		}
	}
	childPath := append(append([]string{}, path...), key)
	if summary, ok := machineBuildCacheSummary(childPath, value, automatic); ok {
		return summary
	}
	return valueSummary(path, key, value, draft, automatic)
}

// machineBuildCacheSummary is the configured policy only: opening the page
// resolves the host's actual defaults. A host the preview found unable to
// share the cache reports `unavailable`, which is the same fact the build
// cache page shows as an unchecked switch.
func machineBuildCacheSummary(path []string, value any, unavailable *string) (string, bool) {
	configured, ok := client.MachineBuildCacheSummary(path, value)
	if !ok {
		return "", false
	}
	if unavailable != nil {
		configured = *unavailable
	}
	return configured + "  ›", true
}

// valueSummary is what a value row shows. automatic replaces the
// placeholder for an unset value when its resolved default is known.
func valueSummary(path []string, key string, value, draft any, automatic *string) string {
	childPath := append(append([]string{}, path...), key)
	var summary string
	_, isBool := setupValueBool(value)
	text, isString := setupValueString(value)
	switch {
	case setupValueIsObject(value) || setupValueIsArray(value):
		contents, ok := client.ContentsSummary(childPath, value)
		if !ok {
			panic("a page holds entries")
		}
		summary = contents + "  ›"
	// The machine's own switch is a checkbox, and an unset value means on.
	// A host that cannot support the cache reports an unchecked box
	// through `automatic`, whatever the machine asks for.
	case (isBool || value == nil) && client.IsBuildCacheField(childPath, "enabled"):
		enabled, ok := setupValueBool(value)
		if !ok || enabled {
			if automatic != nil {
				summary = *automatic
			} else {
				summary = chat.CheckboxMarker(true)
			}
		} else {
			summary = chat.CheckboxMarker(false)
		}
	// Every on/off setting is the same checkbox, whichever section holds it.
	case isBool:
		flag, _ := setupValueBool(value)
		summary = chat.CheckboxMarker(flag)
	// The cache size is measured in whole GB, whatever unit the file spells
	// it in. Only a hand-edited invalid value keeps its own text.
	case isString && client.IsBuildCacheBudget(childPath):
		if automatic != nil && *automatic != "Resolving…" && *automatic != "Unknown" {
			summary = *automatic
		} else {
			summary = client.BuildCacheGigabytesLabel(text)
		}
	case value == nil:
		if automatic != nil {
			summary = *automatic
		} else {
			summary = client.NullLabel(childPath, draft)
		}
	// The archive window's live estimate carries the value itself, so it
	// replaces the number as well as the "Never" placeholder.
	case automatic != nil && key == "archive_after_days":
		summary = *automatic
	case slices.Equal(client.StoragePath(childPath), []string{"theme"}):
		summary = draftThemeReport(value, draft)
	default:
		summary = client.ChoiceLabel(childPath, value, draft)
	}
	if !setupValueIsObject(value) && !setupValueIsArray(value) && !isBool &&
		len(client.Choices(client.StoragePath(childPath), draft)) > 0 {
		return chat.ComboBoxDisplayValue(summary)
	}
	return summary
}
