// Port of mj-client/src/settings/schema.rs.

// Editable defaults and labels for every field in the user configuration.

package client

import (
	"encoding/json"
	"fmt"
	"math"
	"runtime"
	"slices"
	"strconv"
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// settingsTasks is the sub-agent categories, in the order a page lists them.
var settingsTasks = []string{"primary", "straightforward", "subtle", "oracle"}

func isSettingsTask(task string) bool { return slices.Contains(settingsTasks, task) }

// Defaults is the fields the section at path always has, with their default
// values, for the stored value there (a jsonvalue.Object).
func Defaults(path []string, value any) any {
	key := ""
	if len(path) > 0 {
		key = path[len(path)-1]
	}
	first := ""
	if len(path) > 0 {
		first = path[0]
	}
	kindOr := func(fallback string) string {
		if kind, ok := settingsString(settingsGet(value, "kind")); ok {
			return kind
		}
		return fallback
	}
	switch {
	case first == "":
		return jsonvalue.Object{
			{Key: "sessions_side", Value: "left"},
			{Key: "spinner", Value: "scan"},
			{Key: "theme", Value: "midnight"},
			{Key: "advanced", Value: jsonvalue.Object{}},
			{Key: "notify", Value: jsonvalue.Object{}},
			{Key: "phone", Value: jsonvalue.Object{}},
			{Key: "review", Value: jsonvalue.Object{}},
			{Key: "github", Value: jsonvalue.Object{}},
			{Key: "guardian", Value: jsonvalue.Object{}},
			{Key: "jev", Value: jsonvalue.Object{}},
			{Key: "quota", Value: jsonvalue.Object{}},
			{Key: "daemon", Value: jsonvalue.Object{}},
			{Key: "sessionwiki", Value: jsonvalue.Object{}},
			{Key: "subagents", Value: jsonvalue.Object{}},
			{Key: "keys", Value: jsonvalue.Object{{Key: "prefix", Value: core.DefaultPrefix}}},
			{Key: "profiles", Value: jsonvalue.Object{}},
			{Key: "machines", Value: jsonvalue.Object{}},
			{Key: "targets", Value: jsonvalue.Object{}},
			{Key: "bundles", Value: jsonvalue.Object{}},
		}
	case first == "guardian" && len(path) == 1:
		g := core.DefaultGuardianConfig()
		return jsonvalue.Object{
			{Key: "enabled", Value: g.Enabled},
			{Key: "local_classifier", Value: g.LocalClassifier},
			{Key: "use_gpu", Value: g.UseGPU},
		}
	case first == "phone":
		return jsonvalue.Object{
			{Key: "enabled", Value: true},
			{Key: "bind", Value: core.DefaultPhoneConfig().Bind},
			{Key: "tailscale_detect", Value: true},
			{Key: "tls_cert", Value: nil},
			{Key: "tls_key", Value: nil},
		}
	case first == "advanced":
		return jsonvalue.Object{
			{Key: "detailed_activity_clocks", Value: false},
			{Key: "session_order", Value: "project"},
			{Key: "symbols", Value: nil},
			{Key: "tool_output", Value: "grouped"},
			{Key: "tool_output_lines", Value: uint64(core.DefaultToolOutputLines)},
			{Key: "auto_afk_minutes", Value: uint64(0)},
		}
	case first == "notify":
		return jsonvalue.Object{
			{Key: "mode", Value: "terminal"},
			{Key: "bell", Value: true},
			{Key: "delay_seconds", Value: uint64(2)},
			{Key: "title", Value: true},
		}
	// Only the section itself: its reviewer entries are SubagentChoice
	// tables, which reject unknown keys.
	case first == "review" && len(path) == 1:
		return jsonvalue.Object{}
	case first == "github" && len(path) == 1:
		return jsonvalue.Object{{Key: "watch", Value: jsonvalue.Object{}}}
	case first == "github" && key == "watch":
		watch := core.DefaultGithubWatchConfig()
		return jsonvalue.Object{
			{Key: "enabled", Value: watch.Enabled},
			{Key: "use_gpu", Value: watch.UseGPU},
			{Key: "interval_seconds", Value: watch.IntervalSeconds},
			{Key: "api_base", Value: watch.APIBase},
		}
	case first == "jev" && len(path) == 1:
		return jsonvalue.Object{{Key: "enabled", Value: true}, {Key: "fastino_key", Value: nil}}
	case first == "quota":
		return jsonvalue.Object{}
	case first == "daemon":
		return jsonvalue.Object{{Key: "runtime", Value: "host"}, {Key: "image", Value: nil}, {Key: "mounts", Value: []any{}}}
	case first == "sessionwiki":
		return jsonvalue.Object{{Key: "archive_after_days", Value: nil}}
	case first == "subagents" && len(path) == 1:
		return jsonvalue.Object{
			{Key: "max_concurrent", Value: uint64(core.DefaultSubagentConfig().MaxConcurrent)},
			{Key: "primary", Value: []any{}},
			{Key: "straightforward", Value: []any{}},
			{Key: "subtle", Value: []any{}},
			{Key: "oracle", Value: []any{}},
		}
	case first == "subagents" && len(path) == 3 && isSettingsTask(path[1]):
		return jsonvalue.Object{{Key: "model", Value: ""}, {Key: "effort", Value: nil}}
	case first == "profiles" && len(path) == 2:
		return profileDefaults(kindOr("codex"))
	case first == "machines" && len(path) == 2:
		return machineDefaults(kindOr("ssh"))
	// The build cache is its own page, and the saved file keeps only the
	// fields that are set, so the page needs these defaults in its own
	// right: the machine's arm above never sees a section that is already
	// there.
	case first == "machines" && len(path) == 3 && key == "build_cache":
		return buildCacheDefaults()
	case first == "machines" && len(path) == 4 && key == "scheduler":
		return schedulerDefaults()
	case first == "targets" && len(path) == 2:
		return targetDefaults(kindOr("podman"))
	case first == "targets" && key == "workspace_storage":
		if kind, _ := settingsString(settingsGet(value, "kind")); kind == "host-helper" {
			return jsonvalue.Object{{Key: "kind", Value: "host-helper"}, {Key: "root", Value: ""}, {Key: "helper", Value: []any{}}}
		}
		return jsonvalue.Object{{Key: "kind", Value: "podman-volume"}}
	case first == "bundles" && len(path) == 2:
		return jsonvalue.Object{{Key: "primary_repo", Value: "main"}, {Key: "repositories", Value: []any{}}}
	case first == "bundles" && len(path) == 4:
		return RepositoryDefault()
	}
	return jsonvalue.Object{}
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
	fields := jsonvalue.Object{
		{Key: "enabled", Value: true},
		{Key: "kind", Value: kind},
		{Key: "home", Value: ""},
		{Key: "environment", Value: jsonvalue.Object{}},
		{Key: "context_window_bytes", Value: nil},
		{Key: "load_balancing_may_consume_credits", Value: false},
		{Key: "mjolnir_guardian", Value: true},
	}
	switch kind {
	case "codex":
		fields.Set("guardian_review_model", nil)
		fields.Set("codex_profile", nil)
	case "claude":
		fields.Set("claude_permission_mode", nil)
	case "antigravity":
		fields.Set("third_party_models", true)
	}
	fields.Set("models", []any{})
	return fields
}

// machineDefaults is the fields one machine kind needs. A machine owns the
// host settings every runtime on it shares, which is why the build cache
// lives here.
func machineDefaults(kind string) any {
	fields := jsonvalue.Object{{Key: "kind", Value: kind}}
	switch kind {
	case "ssh":
		fields.Set("host", "")
		fields.Set("user", nil)
		fields.Set("identity_file", nil)
		fields.Set("extra_args", []any{})
		fields.Set("workspace_prefix", core.DefaultWorkspacePrefix)
		fields.Set("build_cache", buildCacheDefaults())
	default:
		// This machine, which has nothing to connect to.
		fields.Set("build_cache", buildCacheDefaults())
	}
	return fields
}

// targetDefaults is the fields one runtime kind needs. Every runtime names
// the machine it runs on; the host settings are on that machine, not here.
func targetDefaults(kind string) any {
	fields := jsonvalue.Object{
		{Key: "enabled", Value: true},
		{Key: "kind", Value: kind},
		{Key: "machine", Value: core.LocalMachineID},
	}
	if kind == "bare" {
		fields.Set("permissions", nil)
	} else {
		fields.Set("image", DefaultImage)
		fields.Set("pull_policy", "auto")
		fields.Set("platform", nil)
		fields.Set("cpus", nil)
		fields.Set("memory", nil)
		fields.Set("environment", jsonvalue.Object{})
		fields.Set("workspace_storage", jsonvalue.Object{{Key: "kind", Value: "podman-volume"}})
	}
	return fields
}

// Expand inserts omitted optional fields, preserving all stored values.
//
// The fields a section always has come first, in the order listed above, so
// a page's rows keep their places whichever fields the file stores. Anything
// else, such as the entries of a collection, follows in its stored order.
func Expand(value *any, path *[]string) {
	missing := Defaults(*path, *value)
	switch typed := (*value).(type) {
	case jsonvalue.Object:
		// The stored members move into the expanded object, so a shallow
		// copy is enough.
		stored := append(jsonvalue.Object{}, typed...)
		expanded := jsonvalue.Object{}
		for _, member := range missing.(jsonvalue.Object) {
			if found, ok := stored.Get(member.Key); ok {
				stored.Delete(member.Key)
				expanded.Set(member.Key, found)
			} else {
				expanded.Set(member.Key, settingsClone(member.Value))
			}
		}
		for _, member := range stored {
			expanded.Set(member.Key, member.Value)
		}
		for i := range expanded {
			*path = append(*path, expanded[i].Key)
			Expand(&expanded[i].Value, path)
			*path = (*path)[:len(*path)-1]
		}
		*value = expanded
	case []any:
		for index := range typed {
			*path = append(*path, strconv.Itoa(index))
			Expand(&typed[index], path)
			*path = (*path)[:len(*path)-1]
		}
	}
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

// settingsPathIs matches path against pattern, where "*" matches any one
// segment.
func settingsPathIs(path []string, pattern ...string) bool {
	if len(path) != len(pattern) {
		return false
	}
	for i, part := range pattern {
		if part != "*" && part != path[i] {
			return false
		}
	}
	return true
}

// WholeNumberOf is Rust's whole_number (renamed: the WholeNumber type keeps
// the name).
func WholeNumberOf(path []string) (WholeNumber, bool) {
	switch {
	case settingsPathIs(path, "notify", "delay_seconds"):
		return WholeNumber{0, math.MaxUint64, "Enter a whole number of seconds.", true}, true
	case settingsPathIs(path, "subagents", "max_concurrent"):
		return WholeNumber{1, 64, "Enter a whole number from 1 to 64.", true}, true
	case settingsPathIs(path, "advanced", "tool_output_lines"):
		return WholeNumber{0, 50, "Enter a whole number from 0 to 50.", true}, true
	case settingsPathIs(path, "advanced", "auto_afk_minutes"):
		return WholeNumber{0, math.MaxUint32, "Enter a whole number of minutes from 0 to 4294967295.", true}, true
	case settingsPathIs(path, "sessionwiki", "archive_after_days"):
		return WholeNumber{1, math.MaxUint32, "Enter a whole number of days, at least 1. Clear the field to keep every session.", false}, true
	case settingsPathIs(path, "profiles", "*", "context_window_bytes"):
		return WholeNumber{0, math.MaxUint64, "Enter a whole number of bytes.", false}, true
	case settingsPathIs(path, "machines", "*", "build_cache", "scheduler", "cpus"):
		return WholeNumber{1, math.MaxInt64, "Enter a positive whole number of concurrent compile permits.", true}, true
	}
	return WholeNumber{}, false
}

// ParseWholeNumber converts the text typed into a whole-number field. Blank
// means unset (nil).
func ParseWholeNumber(number WholeNumber, text string) (any, error) {
	text = strings.TrimSpace(text)
	if text == "" {
		return nil, nil
	}
	value, err := parseRustUint64(text)
	if err != nil || value < number.Min || value > number.Max {
		return nil, settingsError(number.Message)
	}
	return value, nil
}

// settingsError is a Result<_, String> failure: the error's text is the
// string.
type settingsError string

func (e settingsError) Error() string { return string(e) }

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
	machine, ok := settingsString(settingsPath(draft, "targets", target, "machine"))
	if !ok {
		machine = core.LocalMachineID
	}
	return machine == core.LocalMachineID
}

var settingsLabels = map[string]string{
	"interface":                          "Interface",
	"prefix":                             "Prefix key",
	"sessions_side":                      "Session sidebar position",
	"spinner":                            "Activity animation",
	"advanced":                           "Advanced",
	"detailed_activity_clocks":           "Detailed activity clocks",
	"session_order":                      "Session order",
	"symbols":                            "Symbols",
	"tool_output":                        "Tool calls",
	"tool_output_lines":                  "Tool output lines",
	"auto_afk_minutes":                   "Auto-AFK after (minutes)",
	"notify":                             "Notifications",
	"mode":                               "Notify through",
	"bell":                               "Ring the terminal bell",
	"delay_seconds":                      "Delay before notifying (seconds)",
	"title":                              "Show counts in the terminal title",
	"theme":                              "Theme",
	"phone":                              "Web Access",
	"review":                             "Code Review",
	"watch":                              "Watcher",
	"use_gpu":                            "Use GPU for GitHub relevance",
	"jev":                                "Jev (hosted service)",
	"fastino_key":                        "Fastino key",
	"quota":                              "Quota checks",
	"daemon":                             "Daemon",
	"mounts":                             "Extra host directories",
	"runtime":                            "Run in",
	"sessionwiki":                        "SessionWiki",
	"archive_after_days":                 "Archive after (days)",
	"subagents":                          "Sub-agents",
	"primary":                            "Primary and orchestrator",
	"straightforward":                    "Straightforward",
	"subtle":                             "Subtle",
	"oracle":                             "Oracle",
	"build_cache":                        "Build caches",
	"scheduler":                          "Compile scheduling",
	"directory":                          "Cache directory",
	"max_total_size":                     "Total cache budget (GB)",
	"max_concurrent":                     "Maximum concurrent children",
	"profiles":                           "Agent Profiles",
	"machines":                           "Machines",
	"targets":                            "Runtimes",
	"machine":                            "Machine",
	"bundles":                            "Projects",
	"enabled":                            "Enabled",
	"trigger":                            "Trigger",
	"profile":                            "Agent profile",
	"kind":                               "Type",
	"home":                               "Account directory",
	"environment":                        "Environment variables",
	"context_window_bytes":               "Context budget (bytes)",
	"guardian_review_model":              "Guardian review model",
	"codex_profile":                      "Codex profile",
	"claude_permission_mode":             "Claude permission mode",
	"mjolnir_guardian":                   "Mjolnir guardian",
	"load_balancing_may_consume_credits": "Load balancing may consume credits",
	"third_party_models":                 "Offer Claude and GPT models",
	"models":                             "Offered models",
	"bind":                               "Listen address and port",
	"tailscale_detect":                   "Detect Tailscale",
	"tls_cert":                           "TLS certificate file",
	"tls_key":                            "TLS private key file",
	"model":                              "Model",
	"effort":                             "Effort",
	"image":                              "Container image",
	"pull_policy":                        "Download image policy",
	"platform":                           "CPU platform",
	"cpus":                               "CPU limit",
	"memory":                             "Memory limit",
	"workspace_storage":                  "Workspace storage",
	"host":                               "SSH host",
	"user":                               "SSH username",
	"ssh_user":                           "SSH username",
	"identity_file":                      "SSH key file",
	"extra_args":                         "SSH arguments",
	"ssh_args":                           "SSH arguments",
	"permissions":                        "Agent permissions",
	"workspace_prefix":                   "Workspace directory for bare runtimes",
	"aws_profile":                        "AWS account profile",
	"region":                             "AWS region",
	"launch_template":                    "EC2 launch template",
	"launch_template_version":            "Template version",
	"address_source":                     "Connect using",
	"primary_repo":                       "Main repository",
	"repositories":                       "Repositories",
	"id":                                 "Name",
	"github":                             "Git URL or GitHub owner/repository",
	"guardian":                           "Guardian",
	"local_classifier":                   "Local classifier",
	"local":                              "Local repository directory",
	"destination":                        "Checkout folder",
	"tools_directory":                    "Native tool cache directory",
	"root":                               "Storage directory",
	"helper":                             "Storage helper command",
}

func Label(key string) string {
	if label, ok := settingsLabels[key]; ok {
		return label
	}
	return key
}

func FieldLabel(path []string) string {
	switch {
	case settingsPathIs(path, "github"):
		return "GitHub events"
	case settingsPathIs(path, "guardian", "use_gpu"):
		return "Use GPU for local classifier"
	case settingsPathIs(path, "github", "watch", "use_gpu"):
		return "Use GPU for GitHub relevance"
	case settingsPathIs(path, "machines", "*", "build_cache", "scheduler", "cpus"):
		return "Concurrent compile permits"
	case settingsPathIs(path, "machines", "*", "build_cache", "scheduler", "memory"):
		return "Compile admission budget"
	}
	last := ""
	if len(path) > 0 {
		last = path[len(path)-1]
	}
	return Label(last)
}

// SubagentOptionsPath is the `(_subagent_options task, index)` lookup key
// for a sub-agent array row.
func SubagentOptionsPath(path []string) (task, index string, ok bool) {
	if len(path) >= 3 && path[0] == "subagents" && isSettingsTask(path[1]) {
		return path[1], path[2], true
	}
	return "", "", false
}

// NullLabel is what an empty (JSON null) value means for this setting: the
// effect the value actually has, never a placeholder. A value the runtime
// resolves from the host is filled in by the caller through `value_summary`'s
// `automatic` argument instead, because only the dialog can measure it.
func NullLabel(path []string, draft any) string {
	switch {
	case settingsPathIs(path, "subagents", "*", "*", "model"):
		return "None"
	case settingsPathIs(path, "machines", "*", "build_cache", "tools_directory"):
		return "~/.cache/mjolnir/build on the machine"
	case settingsPathIs(path, "machines", "*", "build_cache", "scheduler", "cpus"):
		return "Available logical CPUs"
	case settingsPathIs(path, "machines", "*", "build_cache", "scheduler", "memory"):
		return "85% of host memory"
	// An empty archive window never archives; there is no hidden number.
	case settingsPathIs(path, "sessionwiki", "archive_after_days"):
		return "Never"
	case settingsPathIs(path, "jev", "fastino_key"):
		return "Shared proxy key"
	// A cleared number takes the setting's own default.
	case settingsPathIs(path, "notify", "delay_seconds"):
		return fmt.Sprintf("%d (default)", core.DefaultNotifyConfig().DelaySeconds)
	case settingsPathIs(path, "subagents", "*", "*", "effort") && isSettingsTask(path[1]):
		if settingsEqual(settingsPath(draft, "_subagent_options", path[1], path[2], EffortsPending), true) {
			return "Discovering efforts…"
		}
		return "Harness default"
	case settingsPathIs(path, "subagents", "max_concurrent"):
		return fmt.Sprintf("%d (default)", core.DefaultSubagentConfig().MaxConcurrent)
	case settingsPathIs(path, "advanced", "tool_output_lines"):
		return fmt.Sprintf("%d (default)", core.DefaultToolOutputLines)
	case settingsPathIs(path, "advanced", "auto_afk_minutes"):
		return "0 (disabled)"
	// Unset symbols follow the terminal: ASCII on the Linux console or
	// without a UTF-8 locale, Unicode otherwise.
	case settingsPathIs(path, "advanced", "symbols"):
		return "Follows the terminal"
	// The backend emits no CPU or memory flag, so the container competes
	// for the whole machine.
	case settingsPathIs(path, "targets", "*", "cpus"), settingsPathIs(path, "targets", "*", "memory"):
		return "No limit"
	case settingsPathIs(path, "targets", "*", "platform"):
		// A runtime on this machine runs this machine's architecture; one on
		// another machine runs whatever that host is, which setup cannot
		// know.
		if IsLocalRuntime(draft, path[1]) {
			return fmt.Sprintf("Engine default (%s)", rustArch())
		}
		return "Engine default"
	// A bare runtime on an SSH machine takes the guardian default.
	case settingsPathIs(path, "targets", "*", "permissions"):
		return "Ask for approvals"
	case settingsPathIs(path, "machines", "*", "user"):
		return "From ~/.ssh/config, else current user"
	case settingsPathIs(path, "machines", "*", "identity_file"):
		return "OpenSSH default keys"
	case settingsPathIs(path, "machines", "*", "aws_profile"):
		return "AWS CLI default profile"
	case settingsPathIs(path, "machines", "*", "launch_template_version"):
		return "$Default"
	// Until the machine has been inspected, all the screen can say is where
	// the value comes from.
	case settingsPathIs(path, "machines", "*", "build_cache", "*"):
		return "From the machine's own mbx setup"
	case settingsPathIs(path, "phone", "tls_cert"), settingsPathIs(path, "phone", "tls_key"):
		return "None (HTTP only)"
	// A repository needs exactly one of these two, so a blank one is not a
	// default but a choice not yet made.
	case settingsPathIs(path, "bundles", "*", "repositories", "*", "github"),
		settingsPathIs(path, "bundles", "*", "repositories", "*", "local"):
		return "Not set"
	case settingsPathIs(path, "profiles", "*", "context_window_bytes"):
		return fmt.Sprintf("%d (%d KiB)", core.DefaultContextBytes, core.DefaultContextBytes/1024)
	case settingsPathIs(path, "profiles", "*", "guardian_review_model"):
		return core.GuardianReviewNewestFlash
	case settingsPathIs(path, "profiles", "*", "codex_profile"):
		return "None — config.toml only"
	case settingsPathIs(path, "profiles", "*", "claude_permission_mode"):
		return "Auto"
	}
	return "Not set"
}

// rustArch is Rust's std::env::consts::ARCH for this build's architecture.
func rustArch() string {
	switch runtime.GOARCH {
	case "amd64":
		return "x86_64"
	case "arm64":
		return "aarch64"
	case "386":
		return "x86"
	case "arm":
		return "arm"
	case "riscv64":
		return "riscv64"
	case "ppc64le", "ppc64":
		return "powerpc64"
	case "s390x":
		return "s390x"
	case "loong64":
		return "loongarch64"
	}
	return runtime.GOARCH
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
	section := settingsGet(draft, key)
	switch key {
	case "interface":
		return fmt.Sprintf("%s · sidebar %s", theme(settingsGet(draft, "theme"), draft),
			strings.ToLower(ChoiceLabel([]string{"sessions_side"}, settingsGet(draft, "sessions_side"), draft))), true
	case "advanced":
		on := 0
		for _, field := range []string{"detailed_activity_clocks"} {
			if settingsEqual(settingsGet(section, field), true) {
				on++
			}
		}
		order := ""
		if settingsEqual(settingsGet(section, "session_order"), "priority") {
			order = " · priority order"
		}
		tools := ""
		if settingsEqual(settingsGet(section, "tool_output"), "inline") {
			tools = " · inline tools"
		}
		autoAfk := ""
		if minutes, ok := settingsU64(settingsGet(section, "auto_afk_minutes")); ok && minutes > 0 {
			autoAfk = fmt.Sprintf(" · auto-AFK after %d min", minutes)
		}
		if on == 0 {
			return "All off" + order + tools + autoAfk, true
		}
		return fmt.Sprintf("%d on%s%s%s", on, order, tools, autoAfk), true
	case "notify":
		mode, _ := settingsString(settingsGet(section, "mode"))
		switch mode {
		case "off":
			return "Off", true
		case "system":
			return "Desktop and terminal", true
		}
		return "Terminal", true
	case "profiles", "machines", "targets", "bundles":
		return namedEntries(section), true
	case "phone":
		if settingsEqual(settingsGet(section, "enabled"), false) {
			return "Off", true
		}
		if bind, ok := settingsString(settingsGet(section, "bind")); ok {
			return "On · " + bind, true
		}
		return "On", true
	case "review":
		// Review is on exactly when reviewers are configured. A draft that
		// still has to choose them automatically has no list yet.
		reviewers := 0
		if list, ok := settingsArray(settingsGet(section, "reviewers")); ok {
			reviewers = len(list)
		}
		switch {
		case reviewers > 0:
			return fmt.Sprintf("Before push · %d reviewer%s", reviewers, pluralS(reviewers)), true
		case !settingsHas(section, "reviewers") && settingsEqual(settingsGet(section, "trigger"), "push"):
			return "Before push · automatic reviewers", true
		}
		return "Off", true
	case "github":
		switch {
		case settingsEqual(settingsPath(section, "watch", "enabled"), false):
			return "Off", true
		case !jevEnabled(draft):
			// Config.GithubEventsEnabled: GitHub items are classified by
			// Jev, so the GitHub watcher is off.
			return "Off · Jev is off (Privacy)", true
		}
		return "On · GitHub activity", true
	case "guardian":
		switch {
		case settingsEqual(settingsGet(section, "enabled"), false):
			return "Off", true
		case settingsEqual(settingsGet(section, "local_classifier"), false):
			return "On · utility/ACP only", true
		}
		return "On · local classifier", true
	case "quota":
		return "Host", true
	case "daemon":
		runtimeName, _ := settingsString(settingsGet(section, "runtime"))
		switch runtimeName {
		case "podman":
			return "Podman container", true
		case "docker":
			return "Docker container", true
		}
		return "Host", true
	case "jev":
		if settingsEqual(settingsGet(section, "enabled"), false) {
			return "Off · new sessions send nothing", true
		}
		return "On · sends turn text for classification", true
	case "subagents":
		// A cleared limit is the default one, which still applies. The
		// delegation policy is selected when creating or moving a session.
		limit, ok := settingsU64(settingsGet(section, "max_concurrent"))
		if !ok {
			limit = uint64(core.DefaultSubagentConfig().MaxConcurrent)
		}
		return fmt.Sprintf("Up to %d at once", limit), true
	case "sessionwiki":
		if days, ok := settingsU64(settingsGet(section, "archive_after_days")); ok {
			return fmt.Sprintf("Archives after %d days", days), true
		}
		return "Keeps every session", true
	}
	return "", false
}

// namedEntries is the entries of a section the user names, listed rather
// than counted. Past three the tail becomes a count, so the column stays one
// line.
func namedEntries(value any) string {
	entries, ok := settingsObject(value)
	if !ok || len(entries) == 0 {
		return "None yet"
	}
	names := entries.Keys()
	if len(names) <= 3 {
		return strings.Join(names, ", ")
	}
	return fmt.Sprintf("%s, +%d more", strings.Join(names[:2], ", "), len(names)-2)
}

func ChoiceLabel(path []string, value, draft any) string {
	text, ok := settingsString(value)
	if !ok {
		if value == nil {
			return NullLabel(path, draft)
		}
		return settingsDisplay(value)
	}
	last := ""
	if len(path) > 0 {
		last = path[len(path)-1]
	}
	if _, _, subagent := SubagentOptionsPath(path); text == "" && last == "model" &&
		(subagent || (len(path) == 4 && path[0] == "review" && path[1] == "reviewers")) {
		return "Select a model"
	}
	// Only the theme field: `auto` is also an image pull policy.
	if slices.Equal(StoragePath(path), []string{"theme"}) {
		encoded, _ := json.Marshal(text)
		if choice, err := core.UnmarshalThemeChoiceJSON(encoded); err == nil {
			return choice.Label()
		}
	}
	// A download policy is shown by what it does to this target's own image:
	// `auto` is derived from the image reference, not an alias.
	if settingsPathIs(path, "targets", "*", "pull_policy") {
		var policy core.ImagePullPolicy
		if policy.UnmarshalText([]byte(text)) == nil {
			image, _ := settingsString(settingsPath(draft, "targets", path[1], "image"))
			return policy.Describe(image)
		}
	}
	// An agent kind is named once, by the harness itself.
	if len(path) > 0 && path[0] == "profiles" {
		var kind core.HarnessKind
		if kind.UnmarshalText([]byte(text)) == nil {
			return kind.DisplayName()
		}
	}
	// A machine id is shown as the user named it.
	if last == "machine" {
		return text
	}
	if last == "claude_permission_mode" {
		switch text {
		case "auto":
			return "Auto"
		case "accept-edits":
			return "Accept edits"
		case "manual":
			return "Manual"
		}
		return text
	}
	switch text {
	case "native":
		return "Native"
	case "local":
		return "This machine"
	case "ssh":
		return "SSH host"
	case "aws-ec2":
		return "Amazon EC2"
	case "bare":
		return "Bare checkout"
	case "podman":
		return "Podman"
	case "docker":
		return "Docker"
	case "apple-container":
		return "Apple container"
	case "guardian":
		return "Ask for approvals"
	case "yolo":
		return "Allow all actions"
	case "left":
		return "Left"
	case "right":
		return "Right"
	case "grouped":
		return "Grouped"
	case "inline":
		return "Inline with output"
	case "podman-volume":
		return "Managed volume"
	case "container-layer":
		return "Inside the container"
	case "host-helper":
		return "Custom storage helper"
	case "public-dns":
		return "Public DNS name"
	case "public-ip":
		return "Public IP address"
	case "private-dns":
		return "Private DNS name"
	case "private-ip":
		return "Private IP address"
	}
	return text
}

// OptionLabel is how one of a setting's offered values reads in its list of
// choices. Unset says what it falls back to, and the build cache switch,
// whose unset value means on, says so.
func OptionLabel(path []string, value, draft any) string {
	switch {
	case IsBuildCacheField(path, "enabled"):
		switch value {
		case nil:
			return "Default (on)"
		case true:
			return "On"
		}
		return "Off"
	case value == nil && len(path) >= 2 && path[0] == "subagents" && path[len(path)-1] == "model":
		return "None"
	case value == nil:
		return fmt.Sprintf("Default (%s)", NullLabel(StoragePath(path), draft))
	}
	return ChoiceLabel(path, value, draft)
}

// FieldText is the text a typed setting's field holds for value. A build
// cache budget is typed in whole GB, whatever unit the file spells it in.
func FieldText(path []string, value any) string {
	if value == nil {
		return ""
	}
	if size, ok := settingsString(value); ok && IsBuildCacheBudget(path) {
		return BuildCacheGigabytesLabel(size)
	}
	if text, ok := settingsString(value); ok {
		return text
	}
	return settingsDisplay(value)
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
	key := ""
	if len(path) > 0 {
		key = path[len(path)-1]
	}
	task, index, subagent := SubagentOptionsPath(path)
	reviewer := len(path) == 4 && path[0] == "review" && path[1] == "reviewers"
	if (subagent || reviewer) && (key == "model" || key == "effort") {
		var choices []any
		if key == "model" {
			current, found := settingsPointer(draft, Pointer(path))
			model, isString := settingsString(current)
			if !found || !isString || strings.TrimSpace(model) == "" {
				choices = []any{""}
			} else {
				choices = []any{}
			}
		} else {
			choices = []any{nil}
		}
		var options any
		if subagent {
			options = settingsPath(draft, "_subagent_options", task, index)
		} else {
			options = settingsPath(draft, "_reviewer_options", path[2])
		}
		list := "efforts"
		if key == "model" {
			list = "models"
		}
		if values, ok := settingsArray(settingsGet(options, list)); ok {
			for _, value := range values {
				if text, ok := settingsString(settingsGet(value, "value")); ok && text != "adaptive" {
					choices = append(choices, text)
				}
			}
		}
		return choices
	}
	var values []string
	switch {
	case key == "sessions_side":
		values = []string{"left", "right"}
	case key == "session_order":
		values = []string{"project", "priority"}
	case key == "symbols":
		values = []string{"unicode", "ascii"}
	case key == "tool_output":
		values = []string{"grouped", "inline"}
	case key == "runtime" && slices.Equal(path, []string{"daemon", "runtime"}):
		values = []string{"host", "podman", "docker"}
	case key == "mode" && len(path) > 0 && path[0] == "notify":
		values = []string{"off", "terminal", "system"}
	case key == "spinner":
		// Use the canonical animation list below.
	case key == "permissions":
		values = []string{"guardian", "yolo"}
	case key == "claude_permission_mode":
		values = []string{"auto", "accept-edits", "manual"}
	case key == "pull_policy":
		values = []string{"auto", "always", "newer", "missing", "never"}
	case key == "address_source":
		values = []string{"public-dns", "public-ip", "private-dns", "private-ip"}
	case key == "kind" && slices.Contains(path, "workspace_storage"):
		values = []string{"podman-volume", "host-helper", "container-layer"}
	case key == "kind" && len(path) > 0 && path[0] == "profiles":
		values = []string{"codex", "claude", "kimi", "grok", "muse", "opencode", "dsh"}
	case key == "kind" && len(path) > 0 && path[0] == "machines":
		values = []string{"local", "ssh"}
	case key == "kind":
		values = []string{"bare", "podman", "docker", "apple-container"}
	}
	if len(path) == 1 && key == "theme" {
		var choices []any
		for _, choice := range core.ThemeChoiceAll() {
			value, err := settingsToValue(choice)
			if err != nil {
				panic(fmt.Sprintf("theme serializes: %v", err))
			}
			choices = append(choices, value)
		}
		return choices
	}
	if key == "spinner" {
		first := core.DefaultSpinnerStyle()
		current := first
		var choices []any
		for {
			choices = append(choices, string(current))
			current = current.Next()
			if current == first {
				return choices
			}
		}
	}
	if key == "machine" {
		choices := []any{core.LocalMachineID}
		if entries, ok := settingsObject(settingsGet(draft, "machines")); ok {
			for _, id := range entries.Keys() {
				if id != core.LocalMachineID {
					choices = append(choices, id)
				}
			}
		}
		return choices
	}
	// Unset is a choice of its own, so the setting can go back to following
	// the terminal after ASCII or Unicode was picked.
	if key == "symbols" && len(path) > 0 && path[0] == "advanced" {
		return []any{nil, "unicode", "ascii"}
	}
	if key == "profile" {
		choices := []any{nil}
		if entries, ok := settingsObject(settingsGet(draft, "profiles")); ok {
			for _, id := range entries.Keys() {
				choices = append(choices, id)
			}
		}
		return choices
	}
	choices := make([]any, len(values))
	for i, value := range values {
		choices[i] = value
	}
	return choices
}

// jevEnabled is whether the draft leaves the hosted Jev service on.
func jevEnabled(draft any) bool {
	return !settingsEqual(settingsPath(draft, "jev", "enabled"), false)
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
	if len(path) == 1 && path[0] == "subagents" {
		for _, task := range []string{"straightforward", "subtle", "oracle"} {
			if settingsPath(draft, "_subagent_options", task) == nil {
				return "Discovering available subagent models. Unset models remain None until you choose one.", true
			}
		}
	}
	if (len(path) == 1 || len(path) == 2) && path[0] == "github" &&
		(path[len(path)-1] == "github" || path[len(path)-1] == "watch") && !jevEnabled(draft) {
		return "Off because Jev is off: the GitHub watcher needs Jev to classify activity. Mailbox delivery and child reports remain available.", true
	}
	return SpecificHelp(path)
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
	switch {
	case settingsPathIs(path, "github", "watch", "use_gpu"):
		return "Automatically use a GPU only when it runs faster than the CPU. Turn off for CPU-only GitHub relevance scoring. Device failures select CPU and report a notice.", true
	case settingsPathIs(path, "guardian"):
		return "Review permission requests using the local classifier and utility/ACP review path.", true
	case settingsPathIs(path, "guardian", "enabled"):
		return "Controls daemon reviews and new sessions immediately. When off, permission requests are sent to you.", true
	case settingsPathIs(path, "guardian", "local_classifier"):
		return "Controls daemon reviews and new sessions immediately. When off, Stage 1 is skipped and review goes to utility/ACP.", true
	case settingsPathIs(path, "guardian", "use_gpu"):
		return "Use a GPU for the local classifier. This takes effect when the background server next starts.", true
	case settingsPathIs(path, "github"), settingsPathIs(path, "github", "watch"):
		return "Receive GitHub activity selected by Jev. This switch controls only the GitHub event source. Mailbox hooks, peer messages and child reports remain available at tool boundaries and through idle wakes.", true
	case settingsPathIs(path, "machines", "*", "build_cache", "scheduler"):
		return "mbx shares this scheduler across all builds on the machine. Blank fields keep mbx's defaults.", true
	case settingsPathIs(path, "machines", "*", "build_cache", "scheduler", "cpus"):
		return "Machine-wide compile permit count shared by all mbx builds. Must be a positive integer; blank uses mbx's default.", true
	case settingsPathIs(path, "machines", "*", "build_cache", "scheduler", "memory"):
		return "Shared admission budget for compiles (e.g. 8GiB). `none` disables weighting; not a hard cap. Blank keeps mbx default.", true
	}
	last, first := "", ""
	if len(path) > 0 {
		last, first = path[len(path)-1], path[0]
	}
	switch {
	case last == "prefix" && first == "interface":
		return "Key pressed before global shortcuts. Use ctrl, alt, or cmd/super with a key, or use a function key.", true
	case last == "theme":
		return "Auto matches the terminal's background. Terminal Dark and Light use its own colors. Applies after saving.", true
	case last == "profiles":
		return "Add a profile, then choose Sign in. Detect profiles finds installed agents.", true
	case last == "home":
		return "The agent's account directory, such as ~/.codex. Save, then choose Sign in.", true
	case last == "machines":
		return "Add an SSH host or EC2 launch template; saving one adds its runtimes. Its runtimes share its build cache settings.", true
	case last == "targets":
		return "Add a runtime and choose its machine, or use Detect runtimes to probe every machine. Add SSH hosts under Machines.", true
	case last == "machine":
		return "Which machine this runtime runs on.", true
	case last == "phone":
		return "Web access changes take effect when the background server next starts. Remote access requires a certificate and key.", true
	case last == "advanced":
		return "Display options, activity clocks, session order, symbols, tool calls, and automatic AFK.", true
	case last == "notify":
		return "How to be told when a session you are not looking at asks a question, fails, or finishes.", true
	case last == "mode" && first == "notify":
		return "Terminal rings the bell and works over SSH. System also posts a desktop notification.", true
	case last == "bell":
		return "Ring the terminal bell with each notification.", true
	case last == "delay_seconds":
		return "Wait this long before notifying, so a question the agent answers itself stays quiet.", true
	case last == "title":
		return "Keep the terminal window title showing how many sessions are waiting or unread.", true
	case last == "detailed_activity_clocks":
		return "Show elapsed turn and tool clocks in session activity rows.", true
	case last == "session_order":
		return "Group sessions by project, or list the ones that need you first without project headings.", true
	case last == "symbols":
		return "Draw marks and borders in Unicode or ASCII. Unset: ASCII on the Linux console or without UTF-8.", true
	case last == "tool_output":
		return "Grouped folds finished calls into one row of names. Inline shows each call with its command and output, like Codex.", true
	case last == "tool_output_lines":
		return "Output rows under each inline tool call: the first and last lines, with a count of the rest. 0 hides output.", true
	case last == "auto_afk_minutes":
		return "Enter AFK after this many minutes without input in the terminal or web UI. Input resets the timer. 0 disables automatic AFK. Use the AFK return action to come back.", true
	case last == "bundles":
		return "Projects can contain one or more repositories. Choose the main repository where the agent starts.", true
	case last == "repositories":
		return "Set either a local repository directory or a GitHub source for each repository.", true
	case last == "review":
		return "Reviewers for code review before push. With no reviewers, review is off.", true
	case last == "quota":
		return "Quota checks run on this machine, using an installed harness or Mjolnir's pinned installation.", true
	case last == "daemon":
		return "Where the background daemon runs. Saving a change moves it there; running sessions keep going.", true
	case last == "runtime" && first == "daemon":
		return "Host: a process on this computer. Podman or Docker: a container sharing this computer's network and processes, where local sessions run too. Linux only.", true
	case last == "image" && first == "daemon":
		return "The daemon container's image. Blank uses this release's agent-dev image.", true
	case last == "mounts" && first == "daemon":
		return "Directories the daemon container sees, beside your home, Mjolnir's data and settings, and /tmp. Add local projects kept elsewhere.", true
	case last == "jev":
		return "Sends turn text to a hosted classifier; quota recovery needs it. Off stops both; sessions follow on resume or restart.", true
	case last == "fastino_key" && first == "jev":
		return "Optional. Use your own Fastino quota instead of the shared Jev limits. Enter a key, { from_secret = \"NAME\" }, or { from_env = \"NAME\" }. Sessions use it when started or resumed after saving.", true
	case last == "sessionwiki":
		return "Sessions are always indexed into SessionWiki. This page sets archiving; the row below shows what it frees.", true
	case last == "archive_after_days":
		return "After this many days, indexed suspended sessions lose their checkpoint and attachments. Blank keeps all.", true
	case last == "subagents":
		return "Add (model, effort) pairs per category (one per provider). Straightforward enables Mjolnir sub-agents.", true
	case last == "primary":
		return "Optional. Top-level primary sessions (bare `mj new`) and orchestrators.", true
	case last == "straightforward":
		return "Work with a clear method. Add one or more (model, effort) pairs.", true
	case last == "subtle":
		return "Optional. Work needing judgment. Use a smarter model or higher effort than Straightforward.", true
	case last == "oracle":
		return "For read-only tricky questions that the primary agent isn't sure about.", true
	case last == "build_cache":
		return "Share build caches on Linux: mbx for Rust/C/C++, native caches for Go, Gradle, Turbo, Nx and Bazel.", true
	case last == "tools_directory":
		return "Go, Gradle, Turbo and Bazel on local disk. Nx 23.2+ uses ~/.nx. Blank uses ~/.cache/mjolnir/build.", true
	case last == "directory":
		return "The cache directory reported by the machine's native mbx installation. Configure its location on that machine.", true
	case last == "max_total_size":
		return "Budget for compiler outputs, worktrees, and incremental state, in GB. Blank uses the initial automatic budget.", true
	case last == "memory":
		return "Examples: 8g or 4096m. Leave blank for no limit.", true
	case last == "pull_policy":
		return "When Mjolnir downloads this image. The first choice follows the image and never delays a launch.", true
	case last == "context_window_bytes":
		return "Optional positive byte limit for transcript compaction. Leave blank for the default.", true
	case last == "guardian_review_model":
		return "Codex with a custom provider: newest-flash, session (the session's model), or a model name. Blank: newest-flash.", true
	case last == "load_balancing_may_consume_credits":
		return "When this subscription profile has used up its quota for a model, let load balancing still pick it and spend its paid credits. Off: it is skipped until the quota resets. Does not affect a session you pin to this profile.", true
	case last == "models":
		return "Optional. Offer only these models from this profile, one exact model name per entry. Leave empty to offer everything the profile advertises. The profile's current model is always offered.", true
	case last == "codex_profile":
		return "Codex only: the name of a profile file in the account directory, such as ds for ds.config.toml, layered over config.toml like codex -p ds.", true
	case last == "third_party_models":
		return "Antigravity only: offer the Claude and GPT models of the account's third-party quota pool. Off: the profile offers only Gemini models, refuses switching to the others, and hides that pool's quota. A session already on one keeps it until you switch.", true
	case last == "mjolnir_guardian":
		return "On runtimes that keep configured approvals, Mjolnir reviews the tool permission requests that reach it after the harness applies its own rules. The top-level Guardian setting must also be enabled. Off: pending requests go to your permission form. Applies to new sessions.", true
	case last == "claude_permission_mode":
		return "How new sessions on runtimes that ask for approvals handle Claude's permissions. Auto: Claude's classifier approves or denies each action. Accept edits: file edits run, everything else asks you. Manual: every action that needs permission asks you. Runtimes that allow all actions always bypass permissions.", true
	}
	return "", false
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
	if settingsPathIs(path, "phone", "tls_cert") || settingsPathIs(path, "phone", "tls_key") ||
		settingsPathIs(path, "machines", "*", "identity_file") {
		return core.CompletionKindAny
	}
	return core.CompletionKindDirectories
}

// PathKindOf is Rust's path_kind (renamed: the PathKind type keeps the
// name).
func PathKindOf(path []string) (PathKind, bool) {
	switch {
	case settingsPathIs(path, "profiles", "*", "home"),
		settingsPathIs(path, "phone", "tls_cert"),
		settingsPathIs(path, "phone", "tls_key"),
		settingsPathIs(path, "machines", "*", "identity_file"),
		settingsPathIs(path, "bundles", "*", "repositories", "*", "local"),
		settingsPathIs(path, "daemon", "mounts", "*"):
		return PathKindLocal, true
	case settingsPathIs(path, "machines", "*", "workspace_prefix"),
		settingsPathIs(path, "machines", "*", "build_cache", "directory"),
		settingsPathIs(path, "machines", "*", "build_cache", "tools_directory"),
		settingsPathIs(path, "targets", "*", "workspace_storage", "root"):
		return PathKindTarget, true
	case settingsPathIs(path, "bundles", "*", "repositories", "*", "destination"):
		return PathKindRelativeDestination, true
	}
	return 0, false
}
