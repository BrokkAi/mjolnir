// Port of mj-tui/src/setup/page.rs.
//
// Rust's `pub(super) use mj_client::settings::page::{...}` re-exports have no
// Go form: the setup files call client.PagePlan, client.RowLabel,
// client.Breadcrumb and the rest directly.

package tui

import (
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
	panic("unported: mj-tui/src/setup/page.rs value_summary")
}
