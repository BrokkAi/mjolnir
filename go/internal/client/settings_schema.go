// Port of mj-client/src/settings/schema.rs.

// Editable defaults and labels for every field in the user configuration.

package client

import (
	"strconv"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Defaults is the fields the section at path always has, with their default
// values, for the stored value there (a jsonvalue.Object).
func Defaults(path []string, value any) any {
	panic("unported: mj-client/src/settings/schema.rs defaults")
}

func RepositoryDefault() any {
	return jsonvalue.Object{
		{Key: "id", Value: "main"},
		{Key: "github", Value: nil},
		{Key: "local", Value: nil},
		{Key: "destination", Value: "main"},
	}
}

func buildCacheDefaults() any {
	return jsonvalue.Object{
		{Key: "enabled", Value: nil},
		{Key: "directory", Value: nil},
		{Key: "tools_directory", Value: nil},
		{Key: "max_total_size", Value: nil},
		{Key: "scheduler", Value: schedulerDefaults()},
	}
}

func schedulerDefaults() any {
	return jsonvalue.Object{{Key: "cpus", Value: nil}, {Key: "memory", Value: nil}}
}

// profileDefaults is the fields one profile kind uses. Profile validation
// rejects another kind's fields, so the editor never offers them.
func profileDefaults(kind string) any {
	panic("unported: mj-client/src/settings/schema.rs profile_defaults")
}

// machineDefaults is the fields one machine kind needs. A machine owns the
// host settings every runtime on it shares, which is why the build cache
// lives here.
func machineDefaults(kind string) any {
	panic("unported: mj-client/src/settings/schema.rs machine_defaults")
}

// targetDefaults is the fields one runtime kind needs. Every runtime names
// the machine it runs on; the host settings are on that machine, not here.
func targetDefaults(kind string) any {
	panic("unported: mj-client/src/settings/schema.rs target_defaults")
}

// Expand inserts omitted optional fields, preserving all stored values.
//
// The fields a section always has come first, in the order listed above, so
// a page's rows keep their places whichever fields the file stores. Anything
// else, such as the entries of a collection, follows in its stored order.
func Expand(value *any, path *[]string) {
	panic("unported: mj-client/src/settings/schema.rs expand")
}

// WholeNumber is a setting the file stores as a whole number. The form
// edits every value as text, so this is the one place that text becomes a
// number again.
type WholeNumber struct {
	Min uint64
	Max uint64
	// Message is what to say when the text is not a whole number in range.
	Message string
	// Defaulted is whether clearing the field removes it, so the setting's
	// own default applies. Otherwise a cleared field is an optional value
	// set to none.
	Defaulted bool
}

// WholeNumberOf is Rust's whole_number (renamed: the WholeNumber type keeps
// the name).
func WholeNumberOf(path []string) (WholeNumber, bool) {
	panic("unported: mj-client/src/settings/schema.rs whole_number")
}

// ParseWholeNumber converts the text typed into a whole-number field. Blank
// means unset (nil).
func ParseWholeNumber(number WholeNumber, text string) (any, error) {
	panic("unported: mj-client/src/settings/schema.rs parse_whole_number")
}

// IsBuildCacheField is whether a full path names the given field of a
// machine's build cache.
func IsBuildCacheField(path []string, field string) bool {
	return len(path) == 4 && path[0] == "machines" && path[2] == "build_cache" && path[3] == field
}

func IsBuildCacheBudget(path []string) bool {
	return IsBuildCacheField(path, "max_total_size")
}

// IsLocalRuntime is whether the runtime target runs on this machine, which
// is what decides the settings the screen can resolve without asking another
// host.
func IsLocalRuntime(draft any, target string) bool {
	panic("unported: mj-client/src/settings/schema.rs is_local_runtime")
}

func Label(key string) string {
	panic("unported: mj-client/src/settings/schema.rs label")
}

func FieldLabel(path []string) string {
	panic("unported: mj-client/src/settings/schema.rs field_label")
}

// SubagentOptionsKey is the `_subagent_options` key for a sub-agent task
// path. The shared set uses the task name; the YOLO set is prefixed so its
// model's efforts are discovered independently.
func SubagentOptionsKey(path []string) (string, bool) {
	panic("unported: mj-client/src/settings/schema.rs subagent_options_key")
}

// IsYoloSubagentPath is whether a subagent model/effort path belongs to the
// YOLO set.
func IsYoloSubagentPath(path []string) bool {
	return len(path) >= 3 && path[0] == "subagents" && path[1] == "yolo"
}

// SharedSubagents is whether the Sub-agents page uses one set for every
// target. A missing value is the default, which shares.
func SharedSubagents(value any) bool {
	panic("unported: mj-client/src/settings/schema.rs shared_subagents")
}

// NullLabel is what an empty (JSON null) value means for this setting: the
// effect the value actually has, never a placeholder. A value the runtime
// resolves from the host is filled in by the caller through `value_summary`'s
// `automatic` argument instead, because only the dialog can measure it.
func NullLabel(path []string, draft any) string {
	panic("unported: mj-client/src/settings/schema.rs null_label")
}

// SectionSummary is what a section of the first page is actually set to.
//
// The first page lists sections rather than values, and a count of what is
// inside one answers no question anyone has: what a reader wants from "Web
// Access" is whether it is on and where it listens. Absent leaves a section
// to the ordinary value summary.
//
// theme names the draft's theme value. What is actually on screen depends
// on the surface: the terminal resolves Auto against its own background and
// honours `NO_COLOR`, which a browser cannot see.
func SectionSummary(key string, draft any, theme func(value, draft any) string) (string, bool) {
	panic("unported: mj-client/src/settings/schema.rs section_summary")
}

// namedEntries is the entries of a section the user names, listed rather
// than counted. Past three the tail becomes a count, so the column stays one
// line.
func namedEntries(value any) string {
	panic("unported: mj-client/src/settings/schema.rs named_entries")
}

func ChoiceLabel(path []string, value, draft any) string {
	panic("unported: mj-client/src/settings/schema.rs choice_label")
}

// OptionLabel is how one of a setting's offered values reads in its list of
// choices. Unset says what it falls back to, and the build cache switch,
// whose unset value means on, says so.
func OptionLabel(path []string, value, draft any) string {
	panic("unported: mj-client/src/settings/schema.rs option_label")
}

// FieldText is the text a typed setting's field holds for value. A build
// cache budget is typed in whole GB, whatever unit the file spells it in.
func FieldText(path []string, value any) string {
	panic("unported: mj-client/src/settings/schema.rs field_text")
}

// BuildCacheGigabytesLabel is a size string as the whole number of GB the
// field is measured in, or the string itself when it is not a size at all.
func BuildCacheGigabytesLabel(size string) string {
	if gigabytes, ok := core.BuildCacheSizeGigabytes(size); ok {
		return strconv.FormatUint(gigabytes, 10)
	}
	return size
}

func Choices(path []string, draft any) []any {
	panic("unported: mj-client/src/settings/schema.rs choices")
}

// jevEnabled is whether the draft leaves the hosted Jev service on.
func jevEnabled(draft any) bool {
	panic("unported: mj-client/src/settings/schema.rs jev_enabled")
}

// PageHelp is the description drawn above the page path: its Help, except
// where another setting in the draft changes what the page does. The GitHub
// watcher cannot run while Jev is off, whatever its own box says.
func PageHelp(path []string, draft any) string {
	if help, ok := PageSpecificHelp(path, draft); ok {
		return help
	}
	return Help(path)
}

// PageSpecificHelp is PageHelp without the general hint every page without
// help of its own falls back to.
func PageSpecificHelp(path []string, draft any) (string, bool) {
	panic("unported: mj-client/src/settings/schema.rs page_specific_help")
}

// SearchAliases is alternate names for a page, which participate in the
// search's preferred matches.
func SearchAliases(path []string) []string {
	if len(path) == 3 && path[0] == "machines" && path[2] == "build_cache" {
		return []string{"mbx", "cache", "build cache", "Go", "Gradle", "Turbo", "Nx", "Bazel"}
	}
	return nil
}

func Help(path []string) string {
	if help, ok := SpecificHelp(path); ok {
		return help
	}
	if len(path) == 0 || path[len(path)-1] == "" {
		// The first page has no parent setting to describe, so it says what
		// the whole screen does instead.
		return "Changes stay in this draft until you save."
	}
	return "Enter opens or edits a setting. Changes stay in this draft until you save."
}

// SpecificHelp is what the setting at path does, when it has help of its
// own.
func SpecificHelp(path []string) (string, bool) {
	panic("unported: mj-client/src/settings/schema.rs specific_help")
}

// PathKind is which machine's paths a setting names. Path ownership is
// explicit: a similarly named environment key is still text.
type PathKind int

const (
	PathKindLocal PathKind = iota
	PathKindTarget
	PathKindRelativeDestination
)

// CompletionKind is what a local path setting accepts. Three settings name a
// file; every other path setting names a directory.
func CompletionKind(path []string) core.CompletionKind {
	panic("unported: mj-client/src/settings/schema.rs completion_kind")
}

// PathKindOf is Rust's path_kind (renamed: the PathKind type keeps the
// name).
func PathKindOf(path []string) (PathKind, bool) {
	panic("unported: mj-client/src/settings/schema.rs path_kind")
}
