// Port of mj-client/src/settings/page.rs.

// Which rows a settings page lists, under which headings, and what each
// row and page is called.

package client

import (
	"slices"
	"strings"
)

// RootGroups is the first page's groups, in the order they are drawn.
//
// A root key no group names is still listed, under `Other`, so a setting
// added to the configuration can never go missing by being forgotten here.
var RootGroups = []struct {
	Heading string
	Keys    []string
}{
	{"Setup", []string{"profiles", "bundles", "targets", "machines", "daemon"}},
	{"Sessions", []string{"review", "subagents", "quota", "sessionwiki", "phone"}},
	{"Display", []string{"interface", "notify", "advanced"}},
	{"Privacy", []string{"jev"}},
}

// PageRow is a row of a settings page. The first page puts a heading above
// each group and a blank line between them; every page below it is a plain
// list.
type PageRow interface {
	isPageRow()
}

type PageRowHeading struct{ Value string }

type PageRowGap struct{}

// PageRowSetting is an index into the page's keys.
type PageRowSetting struct{ Value int }

func (PageRowHeading) isPageRow() {}
func (PageRowGap) isPageRow()     {}
func (PageRowSetting) isPageRow() {}

// PagePlan is every row paths draws on the page path, in order.
//
// The first page groups its sections by heading. The Sub-agents page puts a
// heading before its conditional YOLO task rows, so the two sets stay
// visibly separate.
func PagePlan(path []string, paths [][]string) []PageRow {
	panic("unported: mj-client/src/settings/page.rs page_plan")
}

// RootGroup is the group heading a first-page row belongs under.
func RootGroup(key string) string {
	for _, group := range RootGroups {
		if slices.Contains(group.Keys, key) {
			return group.Heading
		}
	}
	return "Other"
}

// HiddenRootKey: root keys the page never lists: the file's version, the
// settings the synthetic Interface page gathers, the keybindings, and the
// deprecated stopped-session flag that Advanced now owns.
func HiddenRootKey(key string) bool {
	switch key {
	case "version", "advanced", "sessions_side", "spinner", "theme", "keys",
		"_subagent_options", "_reviewer_options", "_review_action_reason":
		return true
	}
	return false
}

func VisibleKeys(path []string, value any) []string {
	panic("unported: mj-client/src/settings/page.rs visible_keys")
}

// VisiblePaths is the full setting paths shown on a page. Most pages list
// each visible child; the global subagent page presents task model and
// effort fields inline.
func VisiblePaths(path []string, value any) [][]string {
	panic("unported: mj-client/src/settings/page.rs visible_paths")
}

// RowPage is the page that owns the row for a full setting path.
func RowPage(path []string) []string {
	panic("unported: mj-client/src/settings/page.rs row_page")
}

// HiddenKey: settings that exist in the stored shape but have no meaning on
// this entry, so showing them would invite a value that does nothing.
func HiddenKey(path []string, value any, key string) bool {
	panic("unported: mj-client/src/settings/page.rs hidden_key")
}

// IsCollection is whether path lists entries the user named rather than
// fixed settings.
func IsCollection(path []string, value any) bool {
	if _, isArray := value.([]any); isArray {
		return true
	}
	if len(path) == 1 {
		switch path[0] {
		case "profiles", "machines", "targets", "bundles":
			return true
		}
	}
	return len(path) > 0 && path[len(path)-1] == "environment"
}

// RowLabel is the name a row carries on the page path. value nil is Rust's
// None (a JSON null value reads the same here).
//
// The label table is keyed by the last segment of a path, so a name the user
// chose must never be looked up in it: a machine called `local` is that
// machine, not the "Local repository directory" setting that shares the key.
func RowLabel(path []string, parent any, key string, value any) string {
	panic("unported: mj-client/src/settings/page.rs row_label")
}

// Breadcrumb is the trail of page names above path, starting at the first
// page.
//
// Each segment is named the way the page above it names its rows, so a name
// the user chose is shown as they wrote it and only schema keys reach the
// label table: a machine called `local` is that machine, not the "Local
// repository directory" setting that shares the key.
func Breadcrumb(path []string, draft any) string {
	return strings.Join(BreadcrumbSegments(path, draft), " › ")
}

// BreadcrumbSegments is Breadcrumb as separate names: the first page's, then
// one per key of path.
func BreadcrumbSegments(path []string, draft any) []string {
	panic("unported: mj-client/src/settings/page.rs breadcrumb_segments")
}

// MachineBuildCacheSummary is a machine's build cache policy as configured.
// What the host actually resolves an unset value to is only known once a
// surface inspects it.
func MachineBuildCacheSummary(path []string, value any) (string, bool) {
	panic("unported: mj-client/src/settings/page.rs machine_build_cache_summary")
}

// ContentsSummary is how much the page at path holds, counted the way the
// page lists it: hidden keys are left out, and synthetic pages such as
// `interface` count the rows they show.
func ContentsSummary(path []string, value any) (string, bool) {
	panic("unported: mj-client/src/settings/page.rs contents_summary")
}

// RowKind is how a row of a settings page is edited.
type RowKind interface {
	isRowKind()
}

// RowKindPage opens a page of its own.
type RowKindPage struct{}

type RowKindCheckbox struct{}

// RowKindChoices is one of these values; nil (JSON null) means unset.
type RowKindChoices struct{ Value []any }

// RowKindField is text, a number or a path, typed.
type RowKindField struct{}

func (RowKindPage) isRowKind()     {}
func (RowKindCheckbox) isRowKind() {}
func (RowKindChoices) isRowKind()  {}
func (RowKindField) isRowKind()    {}

// RowKindOf is Rust's row_kind (renamed: the RowKind type keeps the name):
// how the row at path on the page page is edited. A collection page lists
// entries, and every entry opens its own page, even one that holds a single
// value.
func RowKindOf(draft any, page []string, path []string) RowKind {
	panic("unported: mj-client/src/settings/page.rs row_kind")
}
