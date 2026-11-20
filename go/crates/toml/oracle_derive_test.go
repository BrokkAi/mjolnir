package toml

import (
	"encoding/json"
	"math/rand"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/serde"
	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/collections"
	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
)

// The types of the oracle's `derived` module (go/port/oracle/src/
// toml_oracle.rs), declared the way the port declares serde-derived types:
// an `rs` tag with the Rust name of each field, a `serde` tag with its
// attributes, and one registration per type with the container attributes.
// The `derive` request reads a document into one of them with toml and
// writes it back out with each of toml's serializers.

type derivedConfig struct {
	Version  uint32                                       `rs:"version"`
	Name     rs.Option[string]                            `rs:"name" serde:"default,skip_serializing_if=Option::is_none"`
	Port     uint16                                       `rs:"port" serde:"default=toml_oracle::derived::default_port"`
	Tags     []string                                     `rs:"tags" serde:"default,skip_serializing_if=Vec::is_empty"`
	Profiles collections.BTreeMap[string, derivedProfile] `rs:"profiles" serde:"default,skip_serializing_if=BTreeMap::is_empty"`
	Targets  []derivedTarget                              `rs:"targets" serde:"default,skip_serializing_if=Vec::is_empty"`
	Daemon   derivedDaemon                                `rs:"daemon" serde:"default"`
	Theme    rs.Option[derivedTheme]                      `rs:"theme" serde:"default,skip_serializing_if=Option::is_none"`
	Ratio    rs.Option[float64]                           `rs:"ratio" serde:"default,skip_serializing_if=Option::is_none"`
	When     rs.Option[ValueDatetime]                     `rs:"when" serde:"default,skip_serializing_if=Option::is_none"`
	Binding  rs.Option[derivedBinding]                    `rs:"binding" serde:"default,skip_serializing_if=Option::is_none"`
	Adjacent rs.Option[derivedAdjacent]                   `rs:"adjacent" serde:"default,skip_serializing_if=Option::is_none"`
	Extra    rs.Option[Table]                             `rs:"extra" serde:"default,skip_serializing_if=Option::is_none"`
}

type derivedProfile struct {
	Harness  derivedHarness    `rs:"harness"`
	Model    rs.Option[string] `rs:"model" serde:"default,skip_serializing_if=Option::is_none"`
	Enabled  bool              `rs:"enabled" serde:"default=toml_oracle::derived::default_true,skip_serializing_if=toml_oracle::derived::is_true"`
	MaxTurns rs.Option[int64]  `rs:"max_turns" serde:"default,rename=max-turns,alias=max_turns,skip_serializing_if=Option::is_none"`
}

type derivedHarness uint8

const (
	derivedHarness_ClaudeCode derivedHarness = iota
	derivedHarness_Codex
	derivedHarness_OpenCode
)

type derivedSshConnection struct {
	Host string            `rs:"host"`
	User rs.Option[string] `rs:"user" serde:"default,skip_serializing_if=Option::is_none"`
	Port rs.Option[uint16] `rs:"port" serde:"default,skip_serializing_if=Option::is_none"`
}

type derivedTarget interface{ isDerivedTarget() }

type derivedTarget_Local struct {
	Name string `rs:"name"`
}

type derivedTarget_Ssh struct {
	Name string               `rs:"name"`
	Ssh  derivedSshConnection `rs:"ssh" serde:"flatten"`
}

type derivedTarget_Container struct {
	Name  string   `rs:"name"`
	Image string   `rs:"image"`
	Args  []string `rs:"args" serde:"default"`
}

func (*derivedTarget_Local) isDerivedTarget()     {}
func (*derivedTarget_Ssh) isDerivedTarget()       {}
func (*derivedTarget_Container) isDerivedTarget() {}

type derivedDaemon struct {
	RemoteCommand rs.Option[string]        `rs:"remote_command"`
	Listen        rs.Option[derivedListen] `rs:"listen"`
	Verbose       bool                     `rs:"verbose"`
}

type derivedListen interface{ isDerivedListen() }

type derivedListen_Port struct{ F0 uint16 }

type derivedListen_Address struct{ F0 string }

type derivedListen_Detailed struct {
	Host string `rs:"host"`
	Port uint16 `rs:"port"`
}

func (*derivedListen_Port) isDerivedListen()     {}
func (*derivedListen_Address) isDerivedListen()  {}
func (*derivedListen_Detailed) isDerivedListen() {}

type derivedBinding interface{ isDerivedBinding() }

type derivedBinding_One struct{ F0 string }

type derivedBinding_Many struct{ F0 []string }

func (*derivedBinding_One) isDerivedBinding()  {}
func (*derivedBinding_Many) isDerivedBinding() {}

type derivedTheme interface{ isDerivedTheme() }

type derivedTheme_Dark struct{}

type derivedTheme_Light struct{}

type derivedTheme_Custom struct{ F0 string }

type derivedTheme_Rgb struct {
	F0 uint8
	F1 uint8
	F2 uint8
}

type derivedTheme_Named struct {
	Name string `rs:"name"`
	Bold bool   `rs:"bold" serde:"default"`
}

func (*derivedTheme_Dark) isDerivedTheme()   {}
func (*derivedTheme_Light) isDerivedTheme()  {}
func (*derivedTheme_Custom) isDerivedTheme() {}
func (*derivedTheme_Rgb) isDerivedTheme()    {}
func (*derivedTheme_Named) isDerivedTheme()  {}

type derivedAdjacent interface{ isDerivedAdjacent() }

type derivedAdjacent_A struct{}

type derivedAdjacent_B struct{ F0 int64 }

type derivedAdjacent_C struct {
	X int64 `rs:"x"`
}

func (*derivedAdjacent_A) isDerivedAdjacent() {}
func (*derivedAdjacent_B) isDerivedAdjacent() {}
func (*derivedAdjacent_C) isDerivedAdjacent() {}

// derivedHolder holds each enum as the value of a key, where toml reads it
// through its value deserializer rather than the document's.
type derivedHolder struct {
	Theme    rs.Option[derivedTheme]    `rs:"theme" serde:"default,skip_serializing_if=Option::is_none"`
	Target   rs.Option[derivedTarget]   `rs:"target" serde:"default,skip_serializing_if=Option::is_none"`
	Adjacent rs.Option[derivedAdjacent] `rs:"adjacent" serde:"default,skip_serializing_if=Option::is_none"`
	Harness  rs.Option[derivedHarness]  `rs:"harness" serde:"default,skip_serializing_if=Option::is_none"`
	Listen   rs.Option[derivedListen]   `rs:"listen" serde:"default,skip_serializing_if=Option::is_none"`
	Themes   []derivedTheme             `rs:"themes" serde:"default,skip_serializing_if=Vec::is_empty"`
}

var (
	_ = rs.RegisterStruct[derivedConfig]("Config")
	_ = rs.RegisterStruct[derivedProfile]("Profile", "deny_unknown_fields")
	_ = rs.RegisterEnum[derivedHarness]("Harness",
		rs.EnumAttrs(`rename_all = "kebab-case"`),
		rs.Const(derivedHarness_ClaudeCode, "ClaudeCode"),
		rs.Const(derivedHarness_Codex, "Codex"),
		rs.Const(derivedHarness_OpenCode, "OpenCode"),
	)
	_ = rs.RegisterStruct[derivedSshConnection]("SshConnection")
	_ = rs.RegisterEnum[derivedTarget]("Target",
		rs.EnumAttrs(`tag = "kind"`, `rename_all = "snake_case"`),
		rs.Variant[derivedTarget_Local]("Local", rs.ShapeStruct),
		rs.Variant[derivedTarget_Ssh]("Ssh", rs.ShapeStruct),
		rs.Variant[derivedTarget_Container]("Container", rs.ShapeStruct),
	)
	_ = rs.RegisterStruct[derivedDaemon]("Daemon", "default")
	_ = serde.RegisterDefaultOf[derivedDaemon]()
	_ = rs.RegisterEnum[derivedListen]("Listen",
		rs.EnumAttrs("untagged"),
		rs.Variant[derivedListen_Port]("Port", rs.ShapeTuple),
		rs.Variant[derivedListen_Address]("Address", rs.ShapeTuple),
		rs.Variant[derivedListen_Detailed]("Detailed", rs.ShapeStruct),
	)
	_ = rs.RegisterEnum[derivedBinding]("Binding",
		rs.EnumAttrs("untagged"),
		rs.Variant[derivedBinding_One]("One", rs.ShapeTuple),
		rs.Variant[derivedBinding_Many]("Many", rs.ShapeTuple),
	)
	_ = rs.RegisterEnum[derivedTheme]("Theme",
		rs.Variant[derivedTheme_Dark]("Dark", rs.ShapeUnit),
		rs.Variant[derivedTheme_Light]("Light", rs.ShapeUnit),
		rs.Variant[derivedTheme_Custom]("Custom", rs.ShapeTuple),
		rs.Variant[derivedTheme_Rgb]("Rgb", rs.ShapeTuple),
		rs.Variant[derivedTheme_Named]("Named", rs.ShapeStruct),
	)
	_ = rs.RegisterEnum[derivedAdjacent]("Adjacent",
		rs.EnumAttrs(`tag = "t"`, `content = "c"`),
		rs.Variant[derivedAdjacent_A]("A", rs.ShapeUnit),
		rs.Variant[derivedAdjacent_B]("B", rs.ShapeTuple),
		rs.Variant[derivedAdjacent_C]("C", rs.ShapeStruct),
	)
	_ = rs.RegisterStruct[derivedHolder]("Holder")

	_ = serde.RegisterDefault("toml_oracle::derived::default_true", func() bool { return true })
	_ = serde.RegisterSkipSerializingIf("toml_oracle::derived::is_true", func(value *bool) bool { return *value })
	_ = serde.RegisterDefault("toml_oracle::derived::default_port", func() uint16 { return 22 })
)

// deriveRoundTrip is the oracle's `round_trip`.
func deriveRoundTrip[T any](text string) any {
	value, err := FromStr[T](text)
	if err != nil {
		return deErrorReply(err)
	}
	result := map[string]any{
		"debug":            rs.DebugString(value),
		"to_string":        serResult(ToString(&value)),
		"to_string_pretty": serResult(ToStringPretty(&value)),
	}
	if converted, err := ValueTryFrom(&value); err != nil {
		result["value"] = map[string]any{"err": err.Error()}
	} else {
		result["value"] = map[string]any{"ok": toCanonical(converted)}
	}
	return map[string]any{"ok": result}
}

func answerDerive(typeName, text string) any {
	switch typeName {
	case "Config":
		return deriveRoundTrip[derivedConfig](text)
	case "Profile":
		return deriveRoundTrip[derivedProfile](text)
	case "Target":
		return deriveRoundTrip[derivedTarget](text)
	case "Theme":
		return deriveRoundTrip[derivedTheme](text)
	case "Daemon":
		return deriveRoundTrip[derivedDaemon](text)
	case "Adjacent":
		return deriveRoundTrip[derivedAdjacent](text)
	case "Holder":
		return deriveRoundTrip[derivedHolder](text)
	}
	panic("unknown derived type " + typeName)
}

// derivedDocuments are documents for each type, read as that type: valid
// ones in every shape the type takes, and ones wrong in each way serde and
// toml report.
var derivedDocuments = map[string][]string{
	"Config": {
		"version = 1\n",
		"",
		"name = 'x'\n",
		"version = -1\n",
		"version = 4294967296\n",
		"version = 'one'\n",
		"version = 1\nport = 70000\n",
		"version = 1\nport = -1\n",
		"version = 1\nport = 2.5\n",
		"version = 1\nunknown = true\n",
		"version = 1\nname = 'mj'\nport = 8080\ntags = ['a', 'b', \"c d\"]\nratio = 0.25\nwhen = 1979-05-27T07:32:00Z\n",
		"version = 1\nwhen = 'not a date'\n",
		"version = 1\nwhen = 1979-05-27\n",
		"version = 1\nwhen = 07:32:00.5\n",
		"version = 1\ntags = 'a'\n",
		"version = 1\ntags = [1, 2]\n",
		"version = 1\n[profiles.default]\nharness = 'claude-code'\nmodel = 'opus'\n",
		"version = 1\n[profiles.default]\nharness = 'claude-code'\nenabled = false\nmax-turns = 5\n",
		"version = 1\n[profiles.default]\nharness = 'codex'\nmax_turns = 5\n",
		"version = 1\n[profiles.default]\nharness = 'codex'\nmax_turns = 5\nmax-turns = 6\n",
		"version = 1\n[profiles.default]\nharness = 'ClaudeCode'\n",
		"version = 1\n[profiles.default]\nharness = 'open-code'\nextra = 1\n",
		"version = 1\n[profiles.default]\nmodel = 'x'\n",
		"version = 1\n[profiles.a]\nharness = 'codex'\n[profiles.b]\nharness = 'open-code'\nenabled = true\n",
		"version = 1\n[profiles.a]\nharness = 1\n",
		"version = 1\n[[targets]]\nkind = 'local'\nname = 'here'\n",
		"version = 1\n[[targets]]\nkind = 'ssh'\nname = 'there'\nhost = 'example.com'\nuser = 'me'\nport = 2222\n",
		"version = 1\n[[targets]]\nkind = 'ssh'\nname = 'there'\nhost = 'example.com'\nport = 99999\n",
		"version = 1\n[[targets]]\nkind = 'ssh'\nname = 'there'\n",
		"version = 1\n[[targets]]\nkind = 'container'\nname = 'box'\nimage = 'alpine'\nargs = ['-it']\n",
		"version = 1\n[[targets]]\nkind = 'container'\nname = 'box'\n",
		"version = 1\n[[targets]]\nkind = 'boat'\nname = 'x'\n",
		"version = 1\n[[targets]]\nname = 'x'\n",
		"version = 1\n[[targets]]\nkind = 1\nname = 'x'\n",
		"version = 1\ntargets = [{ kind = 'local', name = 'a' }, { kind = 'container', name = 'b', image = 'c' }]\n",
		"version = 1\n[daemon]\nremote_command = 'mj'\nverbose = true\n",
		"version = 1\n[daemon]\nlisten = 8080\n",
		"version = 1\n[daemon]\nlisten = 'localhost:8080'\n",
		"version = 1\n[daemon]\nlisten = { host = 'localhost', port = 8080 }\n",
		"version = 1\n[daemon]\nlisten = { host = 'localhost' }\n",
		"version = 1\n[daemon]\nlisten = 70000\n",
		"version = 1\n[daemon]\nlisten = true\n",
		"version = 1\n[daemon]\nverbose = 'yes'\n",
		"version = 1\n[daemon]\nunknown = 1\n",
		"version = 1\ndaemon = 5\n",
		"version = 1\ntheme = 'Dark'\n",
		"version = 1\ntheme = 'Light'\n",
		"version = 1\ntheme = 'Purple'\n",
		"version = 1\ntheme = { Custom = 'solarized' }\n",
		"version = 1\ntheme = { Rgb = [1, 2, 3] }\n",
		"version = 1\ntheme = { Rgb = [1, 2] }\n",
		"version = 1\ntheme = { Rgb = [1, 2, 300] }\n",
		"version = 1\ntheme = { Named = { name = 'x', bold = true } }\n",
		"version = 1\ntheme = { Named = { bold = true } }\n",
		"version = 1\ntheme = { Named = { name = 'x', unknown = 1 } }\n",
		"version = 1\ntheme = {}\n",
		"version = 1\ntheme = { Dark = {}, Light = {} }\n",
		"version = 1\ntheme = 5\n",
		"version = 1\n[theme]\nCustom = 'x'\n",
		"version = 1\n[theme.Named]\nname = 'n'\n",
		"version = 1\nbinding = 'ctrl-c'\n",
		"version = 1\nbinding = ['ctrl-c', 'esc']\n",
		"version = 1\nbinding = 5\n",
		"version = 1\nbinding = [1]\n",
		"version = 1\nadjacent = { t = 'A' }\n",
		"version = 1\nadjacent = { t = 'B', c = 5 }\n",
		"version = 1\nadjacent = { t = 'C', c = { x = 1 } }\n",
		"version = 1\nadjacent = { c = 5, t = 'B' }\n",
		"version = 1\nadjacent = { t = 'D' }\n",
		"version = 1\nadjacent = { t = 'B' }\n",
		"version = 1\nadjacent = { c = 5 }\n",
		"version = 1\n[extra]\na = 1\nb = { c = [1, 'x'] }\n",
		"version = 1\nextra = 5\n",
		"version = 1\nratio = nan\n",
		"version = 1\nratio = 1\n",
		"version = 1\nratio = -0.0\n",
	},
	"Profile": {
		"harness = 'codex'\n",
		"harness = 'codex'\nenabled = false\n",
		"harness = 'codex'\nunknown = 1\n",
		"",
		"harness = ['codex']\n",
	},
	"Target": {
		"kind = 'local'\nname = 'a'\n",
		"kind = 'ssh'\nname = 'a'\nhost = 'h'\n",
		"kind = 'container'\nname = 'a'\nimage = 'i'\n",
		"name = 'a'\n",
		"kind = 'other'\n",
		"",
	},
	"Theme": {
		"Custom = 'x'\n",
		"Rgb = [1, 2, 3]\n",
		"[Named]\nname = 'x'\n",
		"Dark = []\n",
		"Dark = {}\n",
		"Dark = 1\n",
		"Purple = 1\n",
		"",
		"Custom = 'x'\nLight = []\n",
	},
	"Daemon": {
		"",
		"verbose = true\n",
		"listen = 1\n",
		"listen = [1]\n",
	},
	"Adjacent": {
		"t = 'A'\n",
		"t = 'B'\nc = 1\n",
		"t = 'C'\n[c]\nx = 2\n",
		"",
		"c = 1\n",
	},
	"Holder": {
		"theme = 'Dark'\n",
		"theme = { Custom = 'x' }\n",
		"theme = { Rgb = { 0 = 1, 1 = 2, 2 = 3 } }\n",
		"theme = { Rgb = { 1 = 1, 0 = 2, 2 = 3 } }\n",
		"theme = { Rgb = { a = 1 } }\n",
		"theme = { Rgb = 5 }\n",
		"theme = { Dark = [1] }\n",
		"theme = { Dark = { a = 1 } }\n",
		"theme = { Dark = 5 }\n",
		"theme = { Custom = 5 }\n",
		"theme = { Named = 5 }\n",
		"theme = { Named = { name = 'x', other = 1 } }\n",
		"[[themes]]\nCustom = 'x'\n[[themes]]\nNamed = { name = 'y' }\n",
		"themes = ['Dark', 'Light', { Custom = 'z' }]\n",
		"themes = ['Dark', 5]\n",
		"[theme.Rgb]\n0 = 1\n1 = 2\n2 = 3\n",
		"[[theme.Rgb]]\n[[theme.Rgb]]\n",
		"target = { kind = 'local', name = 'a' }\n",
		"[target]\nkind = 'ssh'\nname = 'a'\nhost = 'h'\nport = 'x'\n",
		"adjacent = { t = 'C', c = { x = 'one' } }\n",
		"harness = 'codex'\n",
		"harness = { codex = 1 }\n",
		"harness = 'Codex'\n",
		"listen = { port = 1, host = 'h' }\n",
		"listen = 1.5\n",
		"theme = 'Dark'\ntheme2 = 1\n",
	},
}

// derivedReplacements are values a mutation puts in place of a value.
var derivedReplacements = []string{
	"1", "-1", "70000", "4294967296", "2.5", "nan", "true", "\"x\"", "'claude-code'", "'local'", "'Dark'",
	"[]", "[1, 2]", "['a']", "{}", "{ a = 1 }", "{ Custom = 'x' }", "1979-05-27", "1979-05-27T07:32:00Z",
}

// mutateDerived returns count documents made from document by replacing
// the value of one `key = value` line with a value of another kind.
func mutateDerived(random *rand.Rand, document string, count int) []string {
	lines := strings.SplitAfter(document, "\n")
	var candidates []int
	for index, line := range lines {
		if strings.Contains(line, " = ") && !strings.HasPrefix(strings.TrimSpace(line), "[") {
			candidates = append(candidates, index)
		}
	}
	if len(candidates) == 0 {
		return nil
	}
	var out []string
	for range count {
		index := candidates[random.Intn(len(candidates))]
		line := lines[index]
		key := line[:strings.Index(line, " = ")]
		mutated := append([]string(nil), lines...)
		mutated[index] = key + " = " + derivedReplacements[random.Intn(len(derivedReplacements))] + "\n"
		out = append(out, strings.Join(mutated, ""))
	}
	return out
}

func TestDiffAgainstRust_derive(t *testing.T) {
	oracle.Check(t, "toml", "derive", func(yield func(any)) {
		random := rand.New(rand.NewSource(2))
		for _, typeName := range []string{"Config", "Profile", "Target", "Theme", "Daemon", "Adjacent", "Holder"} {
			for _, text := range derivedDocuments[typeName] {
				yield(map[string]any{"op": "derive", "type": typeName, "text": text})
			}
			for _, text := range derivedDocuments[typeName] {
				for _, mutated := range mutateDerived(random, text, 3) {
					yield(map[string]any{"op": "derive", "type": typeName, "text": mutated})
				}
			}
		}
	}, func(request json.RawMessage) any {
		var r struct {
			Type string `json:"type"`
			Text string `json:"text"`
		}
		if err := json.Unmarshal(request, &r); err != nil {
			t.Fatal(err)
		}
		return oracle.Catch(func() any { return answerDerive(r.Type, r.Text) })
	})
}
