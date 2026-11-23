// Port of mj-client/src/settings/page.rs.

// Which rows a settings page lists, under which headings, and what each
// row and page is called.

package client

import (
	"fmt"
	"slices"
	"strconv"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
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
	{"Sessions", []string{"guardian", "review", "subagents", "quota", "sessionwiki", "phone"}},
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
// The first page groups its sections by heading.
func PagePlan(path []string, paths [][]string) []PageRow {
	if len(path) > 0 {
		plan := make([]PageRow, len(paths))
		for index := range paths {
			plan[index] = PageRowSetting{Value: index}
		}
		return plan
	}
	var plan []PageRow
	group := ""
	grouped := false
	for index, child := range paths {
		key := ""
		if len(child) > 0 {
			key = child[len(child)-1]
		}
		heading := RootGroup(key)
		if !grouped || group != heading {
			if grouped {
				plan = append(plan, PageRowGap{})
			}
			plan = append(plan, PageRowHeading{Value: heading})
			group, grouped = heading, true
		}
		plan = append(plan, PageRowSetting{Value: index})
	}
	return plan
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
	if len(path) == 0 {
		object, isObject := settingsObject(value)
		// `interface` is synthetic and `advanced` is always offered, so both
		// are listed whether or not the draft stores a key for them.
		present := func(key string) bool {
			if key == "interface" || key == "advanced" {
				return true
			}
			if !isObject {
				return false
			}
			_, found := object.Get(key)
			return found
		}
		var keys []string
		for _, group := range RootGroups {
			for _, key := range group.Keys {
				if present(key) {
					keys = append(keys, key)
				}
			}
		}
		if isObject {
			var ungrouped []string
			for _, key := range object.Keys() {
				if !HiddenRootKey(key) && !slices.Contains(keys, key) {
					ungrouped = append(ungrouped, key)
				}
			}
			keys = append(keys, ungrouped...)
		}
		return keys
	}
	if slices.Equal(path, []string{"interface"}) {
		return []string{"prefix", "sessions_side", "spinner", "theme"}
	}
	switch typed := value.(type) {
	case []any:
		keys := make([]string, len(typed))
		for i := range typed {
			keys[i] = strconv.Itoa(i)
		}
		return keys
	}
	if entries, ok := settingsObject(value); ok {
		var keys []string
		for _, key := range entries.Keys() {
			if key != "version" && !HiddenKey(path, value, key) {
				keys = append(keys, key)
			}
		}
		return keys
	}
	return nil
}

// VisiblePaths is the full setting paths shown on a page. Most pages list
// each visible child; the global subagent page presents task model and
// effort fields inline.
func VisiblePaths(path []string, value any) [][]string {
	keys := VisibleKeys(path, value)
	if slices.Equal(path, []string{"subagents"}) {
		fields := [][]string{
			{"subagents", "max_concurrent"},
			{"subagents", "primary"},
			{"subagents", "straightforward"},
			{"subagents", "subtle"},
			{"subagents", "oracle"},
		}
		for _, child := range VisibleKeys(path, value) {
			switch child {
			case "max_concurrent", "primary", "straightforward", "subtle", "oracle":
				continue
			}
			fields = append(fields, []string{"subagents", child})
		}
		return fields
	}
	paths := make([][]string, len(keys))
	for i, key := range keys {
		paths[i] = append(slices.Clone(path), key)
	}
	return paths
}

// RowPage is the page that owns the row for a full setting path.
func RowPage(path []string) []string {
	if len(path) == 0 {
		return []string{}
	}
	return path[:len(path)-1]
}

// HiddenKey: settings that exist in the stored shape but have no meaning on
// this entry, so showing them would invite a value that does nothing.
func HiddenKey(path []string, value any, key string) bool {
	if key == "_subagent_options" || key == "_reviewer_options" {
		return true
	}
	switch {
	// A legacy per-profile table, read but never saved: not a setting.
	// Fields of other profile kinds never appear: each kind's defaults list
	// only its own (Defaults).
	case settingsPathIs(path, "profiles", "*") && (key == "subagents" || key == "muse_auto_review"):
		return true
	// This machine is always this machine; its type is not a choice.
	case settingsPathIs(path, "machines", core.LocalMachineID):
		return key == "kind"
	// Present only while reviewers still have to be chosen automatically: it
	// is not a setting. Review is on exactly when reviewers exist.
	case settingsPathIs(path, "review"):
		return key == "trigger"
	// Approvals are a remote-machine setting: a bare runtime here always uses
	// the configured approvals.
	case settingsPathIs(path, "targets", "*"):
		machine, ok := settingsString(settingsGet(value, "machine"))
		if !ok {
			machine = core.LocalMachineID
		}
		return key == "permissions" && machine == core.LocalMachineID
	}
	return false
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
	if _, isArray := parent.([]any); isArray {
		if id, ok := settingsString(settingsGet(value, "id")); ok {
			return id
		}
		index, err := parseRustUint64(key)
		if err != nil {
			index = 0
		}
		return fmt.Sprintf("Item %d", index+1)
	}
	if IsCollection(path, parent) {
		return key
	}
	return FieldLabel(append(slices.Clone(path), key))
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
	segments := []string{"Settings"}
	parent := []string{}
	for _, key := range path {
		child := append(slices.Clone(parent), key)
		if value, found := settingsPointer(draft, Pointer(parent)); found {
			childValue, _ := settingsPointer(draft, Pointer(child))
			segments = append(segments, RowLabel(parent, value, key, childValue))
		} else {
			segments = append(segments, Label(key))
		}
		parent = child
	}
	return segments
}

// MachineBuildCacheSummary is a machine's build cache policy as configured.
// What the host actually resolves an unset value to is only known once a
// surface inspects it.
func MachineBuildCacheSummary(path []string, value any) (string, bool) {
	if !settingsPathIs(path, "machines", "*", "build_cache") {
		return "", false
	}
	enabled := "Enabled by default"
	if flag, ok := settingsBool(settingsGet(value, "enabled")); ok {
		if flag {
			enabled = "Enabled"
		} else {
			enabled = "Disabled"
		}
	}
	budget := "automatic budget"
	if size, ok := settingsString(settingsGet(value, "max_total_size")); ok {
		budget = fmt.Sprintf("%s GB budget", BuildCacheGigabytesLabel(size))
	}
	return fmt.Sprintf("%s · %s", enabled, budget), true
}

// ContentsSummary is how much the page at path holds, counted the way the
// page lists it: hidden keys are left out, and synthetic pages such as
// `interface` count the rows they show.
func ContentsSummary(path []string, value any) (string, bool) {
	count := len(VisiblePaths(path, value))
	switch value.(type) {
	case []any:
		return core.Counted(count, "entry", "entries"), true
	}
	if settingsIsObject(value) {
		return core.Counted(count, "setting", "settings"), true
	}
	return "", false
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
	value, found := settingsPointer(draft, Pointer(path))
	if !found {
		panic("a listed setting exists")
	}
	if slices.Equal(path, []string{"jev", "fastino_key"}) {
		return RowKindField{}
	}
	current, pageFound := settingsPointer(draft, Pointer(page))
	collection := pageFound && IsCollection(page, current)
	if settingsIsObject(value) || settingsIsArray(value) || collection {
		return RowKindPage{}
	}
	var choices []any
	if IsBuildCacheField(path, "enabled") {
		choices = []any{nil, true, false}
	} else {
		choices = Choices(StoragePath(path), draft)
	}
	parent := path[:len(path)-1]
	parentValue, _ := settingsPointer(draft, Pointer(parent))
	defaults := Defaults(StoragePath(parent), parentValue)
	if len(choices) > 0 && settingsHasNull(defaults, path[len(path)-1]) && !settingsContains(choices, nil) {
		choices = append([]any{nil}, choices...)
	}
	switch {
	case len(choices) > 0:
		return RowKindChoices{Value: choices}
	case isBool(value):
		return RowKindCheckbox{}
	}
	return RowKindField{}
}

func isBool(value any) bool { _, ok := value.(bool); return ok }

// settingsHasNull is `value.get(key).is_some_and(Value::is_null)`: the
// object has the member and it is null.
func settingsHasNull(value any, key string) bool {
	object, ok := value.(interface{ Get(string) (any, bool) })
	if !ok {
		return false
	}
	member, found := object.Get(key)
	return found && member == nil
}
