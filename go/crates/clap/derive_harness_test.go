// What the derive transcriptions (derive_*_test.go) share: stand-ins for
// the types of other crates their fields use, an encoder that writes a
// parsed value as serde_json writes the Rust one, and the comparison with
// the real derive run by the oracle.

package clap

import (
	"encoding/json"
	"reflect"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
)

// mjCrateVersion is CARGO_PKG_VERSION as `#[command(version)]` reads it in
// the oracle, whose crate is version 0.0.0.
const mjCrateVersion = "0.0.0"

// testHarnessKind stands in for mj_core::config::HarnessKind as clap sees
// it: its FromStr.
type testHarnessKind uint8

var testHarnessKindIds = []string{"codex", "claude", "kimi", "grok", "muse", "opencode", "dsh", "antigravity"}

func testHarnessKindFromStr(value string) (testHarnessKind, error) {
	for i, id := range testHarnessKindIds {
		if id == value {
			return testHarnessKind(i), nil
		}
	}
	// anyhow!("unknown harness kind {value:?}")
	return 0, stringError("unknown harness kind " + strDebug(value))
}

// testSubagentMcpRole stands in for mj_core::subagent::SubagentMcpRole as
// clap sees it: its FromStr and Display.
type testSubagentMcpRole uint8

var testSubagentMcpRoleIds = []string{"parent", "message_only", "fetch_only", "child"}

func testSubagentMcpRoleFromStr(value string) (testSubagentMcpRole, error) {
	switch value {
	case "parent", "fixed_parent":
		return 0, nil
	case "message_only":
		return 1, nil
	case "fetch_only":
		return 2, nil
	case "child":
		return 3, nil
	}
	// anyhow::bail!("unknown sub-agent MCP role {other:?}")
	return 0, stringError("unknown sub-agent MCP role " + strDebug(value))
}

// serdeValue encodes a transcribed value as serde's derive serializes the
// Rust one: structs as objects of their Rust field names, Option as null or
// the value, enums externally tagged (a unit variant is its name, a newtype
// variant {"Name": value}, a struct variant {"Name": {..}}), the stand-ins
// as their ids. Unexported fields are read through reflection.
func serdeValue(v reflect.Value) any {
	switch v.Type() {
	case reflect.TypeFor[testHarnessKind]():
		return testHarnessKindIds[v.Uint()]
	case reflect.TypeFor[testSubagentMcpRole]():
		return testSubagentMcpRoleIds[v.Uint()]
	}
	if rsOption, ok := serdeOption(v); ok {
		return rsOption
	}
	switch v.Kind() {
	case reflect.Bool:
		return v.Bool()
	case reflect.String:
		return v.String()
	case reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64:
		return v.Int()
	case reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64:
		return v.Uint()
	case reflect.Float64, reflect.Float32:
		return v.Float()
	case reflect.Slice:
		out := []any{}
		for i := 0; i < v.Len(); i++ {
			out = append(out, serdeValue(v.Index(i)))
		}
		return out
	case reflect.Interface:
		variant := v.Elem().Elem()
		name := variant.Type().Name()
		name = name[strings.LastIndex(name, "_")+1:]
		switch {
		case variant.NumField() == 0:
			return name
		case variant.NumField() == 1 && variant.Type().Field(0).Name == "F0":
			return map[string]any{name: serdeValue(variant.Field(0))}
		default:
			return map[string]any{name: serdeStruct(variant)}
		}
	case reflect.Struct:
		return serdeStruct(v)
	}
	panic("serdeValue: " + v.Type().String())
}

func serdeStruct(v reflect.Value) map[string]any {
	out := map[string]any{}
	for i := 0; i < v.NumField(); i++ {
		field := v.Type().Field(i)
		out[field.Tag.Get("rs")] = serdeValue(v.Field(i))
	}
	return out
}

// serdeOption reads an rs.Option: (value or nil, true), or false when v is
// not one.
func serdeOption(v reflect.Value) (any, bool) {
	if v.Kind() != reflect.Struct || v.Type().PkgPath() != reflect.TypeFor[rs.Unit]().PkgPath() ||
		!strings.HasPrefix(v.Type().Name(), "Option[") {
		return nil, false
	}
	some := v.FieldByName("some")
	if !some.Bool() {
		return nil, true
	}
	return serdeValue(v.FieldByName("value")), true
}

// modelFromCommand describes a built command for the argument generator.
func modelFromCommand(cmd *Command) *genCmd {
	model := &genCmd{name: cmd.GetName()}
	if short, ok := cmd.GetShortFlag().Get(); ok {
		model.shortFlag = short
	}
	model.longFlag = cmd.GetLongFlag().UnwrapOrDefault()
	model.aliases = cmd.GetAllAliases()
	for _, arg := range cmd.GetArguments() {
		if arg.GetId() == idHelp || arg.GetId() == idVersion {
			continue
		}
		opt := &genOpt{
			id:         arg.GetId().AsStr(),
			short:      arg.GetShort().UnwrapOrDefault(),
			long:       arg.GetLong().UnwrapOrDefault(),
			takesValue: arg.isTakesValueSet(),
			positional: arg.IsPositional(),
			multiple:   arg.isMultiple(),
			required:   arg.IsRequiredSet(),
		}
		if pvs := arg.GetPossibleValues(); len(pvs) > 0 {
			for i := range pvs {
				opt.ok = append(opt.ok, pvs[i].GetName())
			}
			opt.values = append(append([]string{}, opt.ok...), "Bogus", nearMissWord(opt.ok[0]))
		} else {
			switch arg.GetValueParser().TypeId().typeId {
			case reflect.TypeFor[testHarnessKind]():
				opt.ok = []string{"codex", "claude", "opencode"}
				opt.values = append(append([]string{}, opt.ok...), "open-code", "")
			case reflect.TypeFor[testSubagentMcpRole]():
				opt.ok = []string{"parent", "child", "fixed_parent"}
				opt.values = append(append([]string{}, opt.ok...), "Parent", "x")
			case reflect.TypeFor[bool]():
				opt.ok = []string{"true", "false"}
				opt.values = append(append([]string{}, opt.ok...), "yes", "")
			case reflect.TypeFor[int](), reflect.TypeFor[int32](), reflect.TypeFor[uint64](), reflect.TypeFor[uint32]():
				opt.ok = []string{"3", "0", "4096"}
				opt.values = append(append([]string{}, opt.ok...), "-1", "x", "2147483648")
			default:
				opt.ok = []string{"/root/a", "x", "s1"}
				opt.values = append(append([]string{}, opt.ok...), "", "-", "日本", "--json")
			}
		}
		model.opts = append(model.opts, opt)
	}
	for _, sub := range cmd.GetSubcommands() {
		model.subs = append(model.subs, modelFromCommand(sub))
	}
	return model
}

func nearMissWord(s string) string {
	if len(s) < 2 {
		return s + "x"
	}
	return s[1:2] + s[:1] + s[2:]
}

// subcommandPaths lists the paths to every subcommand, depth first.
func subcommandPaths(model *genCmd, prefix []string) [][]string {
	var paths [][]string
	for _, sub := range model.subs {
		path := append(append([]string{}, prefix...), sub.name)
		paths = append(paths, path)
		paths = append(paths, subcommandPaths(sub, path)...)
	}
	return paths
}

// derivedCases yields the requests for one derive transcription: batches of
// argument vectors (valid ones, every kind of mistake, help and version at
// each level) and the rendered help of every subcommand.
func derivedCases(op string, command func() *Command, batches int, extra [][]string) func(yield func(any)) {
	return func(yield func(any)) {
		cmd := command()
		cmd.Build()
		model := modelFromCommand(cmd)
		if len(extra) > 0 {
			yield(map[string]any{"op": op + "_runs", "argvs": extra})
		}
		for seed := 0; seed < batches; seed++ {
			g := newSpecGen(uint64(seed) + 7)
			var argvs [][]string
			for i := 0; i < 20; i++ {
				var argv []string
				if i%2 == 0 {
					argv = g.genValidArgv(model)
				} else {
					argv = g.genArgv(model)
				}
				argv[0] = cmd.GetName()
				argvs = append(argvs, argv)
			}
			yield(map[string]any{"op": op + "_runs", "argvs": argvs})
		}
		paths := append([][]string{{}}, subcommandPaths(model, nil)...)
		for _, path := range paths {
			for _, what := range []string{"help", "long_help", "usage"} {
				yield(map[string]any{"op": op + "_render", "path": path, "what": what})
			}
		}
		yield(map[string]any{"op": op + "_render", "path": []string{}, "what": "version"})
	}
}

// answerDerived is the Go side of a derive transcription's ops.
func answerDerived[T any](op string, command func() *Command, parser Parser[T]) func(json.RawMessage) any {
	parse := func(argv []string) any {
		value, err := parser.TryParseFrom(argv)
		if err != nil {
			return map[string]any{"err": dumpError(err)}
		}
		return map[string]any{"ok": serdeValue(reflect.ValueOf(value))}
	}
	return func(raw json.RawMessage) any {
		var req specRequest
		if err := json.Unmarshal(raw, &req); err != nil {
			return map[string]any{"oracle_error": err.Error()}
		}
		return withEnv(req.Env, func() any {
			return catchPanic(func() any {
				switch req.Op {
				case op:
					return parse(req.Argv)
				case op + "_runs":
					results := []any{}
					for _, argv := range req.Argvs {
						results = append(results, parse(argv))
					}
					return map[string]any{"results": results}
				case op + "_render":
					return specRender(command(), req.Path, req.What)
				}
				return map[string]any{"oracle_error": "unknown op " + req.Op}
			})
		})
	}
}

func checkDerived[T any](t *testing.T, op string, command func() *Command, parser Parser[T], batches int, extra [][]string) {
	t.Helper()
	oracle.Check(t, "clap", op, derivedCases(op, command, batches, extra), answerDerived(op, command, parser))
}
