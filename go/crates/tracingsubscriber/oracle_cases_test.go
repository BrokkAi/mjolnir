package tracingsubscriber

import (
	"encoding/json"
	"strconv"
	"strings"
)

// The requests the differential tests send. Generation is deterministic: the
// fixed cases first, then cases drawn from a seeded generator, so a recorded
// corpus is reproducible.

// generator is a small fixed random source (splitmix64), so the corpus does
// not depend on the Go release.
type generator struct{ state uint64 }

func (g *generator) next() uint64 {
	g.state += 0x9e3779b97f4a7c15
	z := g.state
	z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9
	z = (z ^ (z >> 27)) * 0x94d049bb133111eb
	return z ^ (z >> 31)
}

func (g *generator) below(n int) int { return int(g.next() % uint64(n)) }

func (g *generator) chance(percent int) bool { return g.below(100) < percent }

func pick[T any](g *generator, items []T) T { return items[g.below(len(items))] }

func ptr[T any](value T) *T { return &value }

func raw(value any) json.RawMessage {
	encoded, err := json.Marshal(value)
	if err != nil {
		panic(err)
	}
	return encoded
}

var levels = []string{"ERROR", "WARN", "INFO", "DEBUG", "TRACE"}

// targets are module paths as Mjolnir writes them, the targets its filters
// name, and prefixes of them.
var targets = []string{
	"mj_core::runtime", "mj_core", "mj_jev", "mj_jev::verdict", "mj_guardian", "mj_guardian_x",
	"mj_controller::latency", "mj_chat::latency", "mj_worker::relay", "other_crate::module", "mj", "",
}

// texts are the strings fields and messages are made of: the edges of the
// escaping (quotes, backslashes, control characters, the escape sequences
// sanitisation guards against, C1 controls), Unicode, braces and empties.
var texts = []string{
	"", "plain", "two words", "quote \" and backslash \\", "tab\there", "line\nbreak", "crlf\r\n",
	"esc \x1b[31mred\x1b[0m", "bell \x07 bs \x08 ff \x0c del \x7f", "c1 \u0085 \u009b next",
	"unicode é ñ 日本語 🦀", "combining é", "​zero width", "nul \x00 byte", "{braces} {0}",
	"key=value", "trailing space ", strings.Repeat("long ", 30), "ünïcödé\nsecond line",
}

var spanNames = []string{"quota_probe", "project_memory_io", "span", "outer", "inner", "s", "span-name"}

var fieldNames = []string{
	"error", "session_id", "in_flight", "count", "path", "elapsed_ms", "log.target", "r#type", "a.b",
	"profile_id", "project_key", "x",
}

var fieldKinds = []string{
	"i64", "u64", "f64", "bool", "str", "display", "debug_str", "debug_int", "debug_float", "debug_bool",
	"debug_list_str", "debug_list_int", "debug_opt_str", "debug_opt_int", "debug_unit", "error", "bytes", "empty",
}

var floats = []any{
	0.0, 1.0, -1.5, 0.1, 1e-7, 1.5e16, 123456.789, 1e300, 2.2250738585072014e-308, "NaN", "inf", "-inf", "-0",
	1000000.0, 0.0001, 0.00001,
}

var integers = []int64{0, 1, -1, 42, -9223372036854775808, 9223372036854775807, 1000000}

func (g *generator) fieldValue(kind string) json.RawMessage {
	switch kind {
	case "i64", "debug_int":
		return raw(pick(g, integers))
	case "u64":
		return raw(pick(g, []uint64{0, 1, 18446744073709551615, 4096}))
	case "f64", "debug_float":
		return raw(pick(g, floats))
	case "bool", "debug_bool":
		return raw(g.chance(50))
	case "debug_list_str":
		items := make([]string, g.below(3))
		for i := range items {
			items[i] = pick(g, texts)
		}
		return raw(items)
	case "debug_list_int":
		items := make([]int64, g.below(4))
		for i := range items {
			items[i] = pick(g, integers)
		}
		return raw(items)
	case "debug_opt_str":
		if g.chance(30) {
			return raw(nil)
		}
		return raw(pick(g, texts))
	case "debug_opt_int":
		if g.chance(30) {
			return raw(nil)
		}
		return raw(pick(g, integers))
	case "debug_unit", "empty":
		return raw(nil)
	case "error":
		chain := make([]string, 1+g.below(3))
		for i := range chain {
			chain[i] = pick(g, texts)
		}
		return raw(chain)
	case "bytes":
		bytes := make([]int, g.below(5))
		for i := range bytes {
			bytes[i] = g.below(256)
		}
		return raw(bytes)
	}
	return raw(pick(g, texts))
}

func (g *generator) fields(max int) []fieldSpec {
	count := g.below(max + 1)
	fields := make([]fieldSpec, 0, count)
	used := map[string]bool{}
	for range count {
		name := pick(g, fieldNames)
		if used[name] {
			continue
		}
		used[name] = true
		kind := pick(g, fieldKinds)
		fields = append(fields, fieldSpec{Name: name, Kind: kind, Value: g.fieldValue(kind)})
	}
	return fields
}

var formats = []string{
	"{}", "connected to {} in {}ms", "value={:?} other={:?}", "[{:>8}] [{:<6}] [{:^7}]", "{:05} {:#x} {:.3}",
	"{0} {0} {1}", "multi\nline {}", "{name} is {age}", "{{braces}} {} {{}}",
}

func (g *generator) message() *messageSpec {
	if g.chance(15) {
		return nil
	}
	if g.chance(45) {
		// A literal: braces in it are doubled, as a format string has them.
		literal := pick(g, texts)
		literal = strings.ReplaceAll(strings.ReplaceAll(literal, "{", "{{"), "}", "}}")
		return &messageSpec{Format: "", Literal: literal}
	}
	format := pick(g, formats)
	var args []json.RawMessage
	switch format {
	case "connected to {} in {}ms", "value={:?} other={:?}", "{name} is {age}":
		args = []json.RawMessage{raw(pick(g, texts)), raw(pick(g, integers))}
	case "{:05} {:#x} {:.3}":
		args = []json.RawMessage{raw(pick(g, integers)), raw(pick(g, []int64{0, 255, 4096, 1})), raw(pick(g, floats))}
	default:
		args = []json.RawMessage{raw(pick(g, texts)), raw(pick(g, texts)), raw(pick(g, texts))}
	}
	return &messageSpec{Format: format, Args: args}
}

// filterDirectives are filters as RUST_LOG holds them: Mjolnir's defaults,
// and every kind of directive.
var filterDirectives = []string{
	"info", "warn,mj_jev=info,mj_guardian=info", "trace", "debug", "off", "", "error",
	"mj_core=debug", "mj_core::runtime=trace,warn", "mj=trace", "info,mj_jev=off",
	"[quota_probe]=debug", "info,[quota_probe]=trace", "[project_memory_io{project_key=abc}]=trace",
	"warn,[{x=1}]=debug", "mj_core[outer]=trace", "[outer{session_id}]=debug", "[{error}]=debug",
	"warn,[span{count=42}]=trace", "[inner{in_flight=true}]=trace", "[span{x=plain}]=trace",
	"[span{x=\"plain\"}]=trace", "[{path=.*}]=info", "mj_chat::latency=debug,mj_controller::latency=debug",
	"MJ_CORE=DEBUG", "mj_core=5", "warn,mj_core=wrong", "mj_jev=info=debug",
}

func (g *generator) runConfig(index int) runConfig {
	switch index % 12 {
	case 0:
		// mj-cli: fmt().with_ansi(false).with_env_filter(EnvFilter::new(..)).
		return runConfig{Ansi: ptr(false), Filter: &filterSpec{Directives: ptr(pick(g, filterDirectives)), Mode: "new"}}
	case 1:
		// mj-worker-bin: the worker's default filter.
		return runConfig{Ansi: ptr(false), Filter: &filterSpec{Directives: ptr("warn,mj_jev=info,mj_guardian=info"), Mode: "new"}}
	case 2:
		// Test captures: fmt().with_ansi(false).with_writer(..).finish().
		return runConfig{Ansi: ptr(false)}
	case 3:
		return runConfig{Ansi: ptr(true), Filter: &filterSpec{Directives: ptr("trace"), Mode: "new"}}
	case 4:
		return runConfig{Time: "none", Level: ptr(false), Target: ptr(false), Filter: &filterSpec{Directives: ptr("debug"), Mode: "new"}}
	case 5:
		return runConfig{Ansi: ptr(false), Sanitize: ptr(false), Filter: &filterSpec{Directives: ptr("trace"), Mode: "new"}}
	case 6:
		return runConfig{Ansi: ptr(false), Time: "none", SpanEvents: "new|enter|exit|close", Filter: &filterSpec{Directives: ptr(pick(g, []string{"trace", "info", "[outer]=trace"})), Mode: "new"}}
	case 7:
		return runConfig{Ansi: ptr(false), Filter: &filterSpec{MaxLevel: pick(g, []string{"warn", "trace", "off", "2"})}}
	case 8:
		return runConfig{Ansi: ptr(false), Filter: &filterSpec{Directives: ptr(g.directives()), Mode: "try_new", Regex: ptr(g.chance(80))}}
	case 9:
		return runConfig{Ansi: ptr(true), SpanEvents: "new|exit", Time: "none", Filter: &filterSpec{Directives: ptr(pick(g, filterDirectives)), Mode: "new"}}
	case 10:
		return runConfig{Ansi: ptr(false), Filter: &filterSpec{Directives: ptr(pick(g, filterDirectives)), Mode: "new", Regex: ptr(false)}}
	}
	return runConfig{Ansi: ptr(g.chance(50)), Filter: &filterSpec{Directives: ptr(pick(g, filterDirectives)), Mode: "new"}}
}

// script makes a script of spans and events, nesting spans up to three
// deep and closing every span at the end (Rust drops what is left open in an
// order of its own).
func (g *generator) script() []runAction {
	var actions []runAction
	var open []string    // spans made and not closed
	var entered []string // spans entered, innermost last
	nextSpan := 0
	steps := 2 + g.below(10)
	for range steps {
		switch choice := g.below(100); {
		case choice < 50:
			action := runAction{
				Do:      "event",
				Level:   pick(g, levels),
				Target:  pick(g, targets),
				Message: g.message(),
				Fields:  g.fields(3),
			}
			if g.chance(15) {
				if len(open) > 0 && g.chance(60) {
					action.Parent = pick(g, open)
				} else {
					action.Parent = "root"
				}
			}
			actions = append(actions, action)
		case choice < 70 && len(entered) < 3:
			id := "s" + strconv.Itoa(nextSpan)
			nextSpan++
			action := runAction{
				Do:     "span",
				Id:     id,
				Name:   pick(g, spanNames),
				Level:  pick(g, levels),
				Target: pick(g, targets),
				Fields: g.fields(2),
			}
			if g.chance(15) {
				if len(open) > 0 && g.chance(50) {
					action.Parent = pick(g, open)
				} else {
					action.Parent = "root"
				}
			}
			actions = append(actions, action)
			open = append(open, id)
			if g.chance(85) {
				actions = append(actions, runAction{Do: "enter", Id: id})
				entered = append(entered, id)
			}
		case choice < 78 && len(entered) > 0:
			id := entered[len(entered)-1]
			entered = entered[:len(entered)-1]
			actions = append(actions, runAction{Do: "exit", Id: id})
		case choice < 86 && len(open) > 0:
			kind := pick(g, fieldKinds)
			actions = append(actions, runAction{
				Do: "record", Id: pick(g, open), Name: pick(g, fieldNames), Kind: kind, Value: g.fieldValue(kind),
			})
		case choice < 93:
			actions = append(actions, runAction{Do: "enabled", Level: pick(g, levels), Target: pick(g, targets)})
		default:
			if len(entered) == 0 && len(open) > 0 {
				id := open[len(open)-1]
				open = open[:len(open)-1]
				actions = append(actions, runAction{Do: "close", Id: id})
			}
		}
	}
	for len(entered) > 0 {
		id := entered[len(entered)-1]
		entered = entered[:len(entered)-1]
		actions = append(actions, runAction{Do: "exit", Id: id})
	}
	for len(open) > 0 {
		id := open[len(open)-1]
		open = open[:len(open)-1]
		actions = append(actions, runAction{Do: "close", Id: id})
	}
	return actions
}

func runCases(yield func(any)) {
	g := &generator{state: 20261120}

	// Fixed cases: one line of each shape Mjolnir writes.
	yield(runRequest{Op: "run", Config: runConfig{Ansi: ptr(false), Filter: &filterSpec{Directives: ptr("info"), Mode: "new"}},
		Actions: []runAction{
			{Do: "event", Level: "INFO", Target: "mjolnir::logging", Message: &messageSpec{Format: "", Literal: "Mjolnir started"}, Fields: []fieldSpec{
				{Name: "version", Kind: "str", Value: raw("2.73.0")},
				{Name: "process_id", Kind: "u64", Value: raw(4242)},
				{Name: "command", Kind: "str", Value: raw("daemon-run")},
				{Name: "log", Kind: "display", Value: raw("/home/user/.local/share/mjolnir/logs/mj-daemon.log")},
			}},
			{Do: "event", Level: "WARN", Target: "mjolnir::logging", Message: &messageSpec{Format: "", Literal: "ignored invalid RUST_LOG filter"}, Fields: []fieldSpec{
				{Name: "error", Kind: "display", Value: raw("invalid filter directive")},
			}},
			{Do: "event", Level: "WARN", Target: "mj_core::runtime", Message: nil, Fields: []fieldSpec{
				{Name: "reason", Kind: "display", Value: raw("shutting down")},
			}},
			{Do: "event", Level: "DEBUG", Target: "mj_core::runtime", Message: &messageSpec{Format: "", Literal: "filtered out"}},
		}})
	yield(runRequest{Op: "run", Config: runConfig{Ansi: ptr(false), Filter: &filterSpec{Directives: ptr("warn,mj_jev=info,mj_guardian=info"), Mode: "new"}},
		Actions: []runAction{
			{Do: "span", Id: "q", Name: "quota_probe", Level: "INFO", Target: "mj_accounts::quota", Fields: []fieldSpec{{Name: "profile_id", Kind: "display", Value: raw("claude-1")}}},
			{Do: "enter", Id: "q"},
			{Do: "event", Level: "WARN", Target: "mj_accounts::quota", Message: &messageSpec{Format: "", Literal: "quota refresh task failed"}, Fields: []fieldSpec{{Name: "error", Kind: "error", Value: raw([]string{"task failed", "connection reset"})}}},
			{Do: "event", Level: "INFO", Target: "mj_jev", Message: &messageSpec{Format: "", Literal: "GLiDE classification requested"}, Fields: []fieldSpec{{Name: "request_id", Kind: "u64", Value: raw(7)}}},
			{Do: "event", Level: "INFO", Target: "mj_core", Message: &messageSpec{Format: "", Literal: "not shown"}},
			{Do: "exit", Id: "q"},
			{Do: "close", Id: "q"},
		}})

	for index := range 640 {
		yield(runRequest{Op: "run", Config: g.runConfig(index), Actions: g.script()})
	}
}

// ===== directive text =====

var directiveTargets = []string{
	"mj_core", "mj_core::runtime", "target-name", "a", "crate1::mod1", "MJ_CORE", "日本", "é", "_x", "x:", "1",
}

var directiveLevels = []string{
	"info", "INFO", "wArN", "debug", "trace", "error", "off", "OFF", "0", "1", "3", "5", "6", "", "wrong",
	"noNumber", " info", "info ", "-1", "+3", "03",
}

// fieldSpecs are field filters. The oracle is built without
// regex-automata's Unicode features, which Mjolnir's build has (other crates
// enable them), so patterns that need them (\d, \w, \s, (?i), \b) answer
// differently there and are tested against Mjolnir's build in
// filter_env_field_test.go instead.
var fieldSpecs = []string{
	"x", "x=1", "x=-1", "x=1.5", "x=true", "x=false", "x=NaN", "x=inf", "x=plain", "x=\"plain\"", "x=a.*", "x=(",
	"x=[a-z]+", "x=[0-9]+", "x=wo+rd", "x=a=b", "count=42", "session_id=abc", "field=\"value\"", "x=", "=1",
	"x=18446744073709551615", "x=18446744073709551616", "x=1e3", "x=a|bc", "x=é+", "x=\\p{Greek}", "x=.*n.*",
}

func (g *generator) directive() string {
	var b strings.Builder
	if g.chance(5) {
		b.WriteString(pick(g, []string{" ", "  ", "\t", " é"}))
	}
	if g.chance(70) {
		b.WriteString(pick(g, directiveTargets))
	}
	if g.chance(40) {
		b.WriteString("[")
		if g.chance(70) {
			b.WriteString(pick(g, spanNames))
		}
		if g.chance(60) {
			b.WriteString("{")
			b.WriteString(pick(g, fieldSpecs))
			if g.chance(15) {
				b.WriteString(" " + pick(g, fieldSpecs))
			}
			b.WriteString("}")
		}
		b.WriteString("]")
	}
	if g.chance(80) {
		if b.Len() > 0 || g.chance(50) {
			b.WriteString("=")
		}
		b.WriteString(pick(g, directiveLevels))
	}
	if g.chance(4) {
		b.WriteString(pick(g, []string{"]", "{", "}", "=", "[", " ", "!"}))
	}
	return b.String()
}

func (g *generator) directives() string {
	parts := make([]string, 1+g.below(4))
	for i := range parts {
		parts[i] = g.directive()
	}
	return strings.Join(parts, ",")
}

// crateDirectives are the directive strings of the crate's own tests.
var crateDirectives = []string{
	"common=trace,server=trace", "common=INFO,server=DEBUG", "common=iNfo,server=dEbUg",
	"crate1::mod1=error,crate1::mod2,crate2=debug,crate3=off",
	"crate1::mod1=error,crate1::mod2=warn,crate1::mod2::mod3=info,crate2=debug,crate3=trace,crate3::mod2::mod1=off",
	"crate1::mod1=1,crate1::mod2=2,crate1::mod2::mod3=3,crate2=4,crate3=5,crate3::mod2::mod1=0",
	"crate1::mod1=warn=info,crate2=debug", "crate1::mod1=noNumber,crate2=debug", "crate1::mod1=wrong,crate2=warn",
	"crate1::mod1=wrong,crate2=", "warn,crate2=debug", "warn", "WARN", "wArN",
	"crate1::mod1[foo]=error,crate1::mod2[bar],crate2[baz]=debug", "target-name=info", "target[span-name]=info",
	"target[!\"#$%&'()*+-./:;<=>?@^_`|~[}]=info", "target[]{]=info",
	"[span1{foo=1}]=error,[span2{bar=2 baz=false}],crate2[{quux=\"quuux\"}]=debug",
	"foo::bar=debug,foo::bar::baz=trace,foo=info,a_really_long_name_with_no_colons=warn",
	"bar[span]=trace,foo=debug,baz::quux=info,a[span]=warn", "span[b]=debug,b=debug,a=trace,c=info,span[a]=info",
	"app=debug", "app=off", "app[mySpan]=debug", "app[mySpan{field=\"value\"}]=debug",
	"app[mySpan{field=\"value\",field2=2}]=debug", "foo=info,foo=debug", "foo=trace,foo=error", ",,,", "",
}

func filterCases(yield func(any)) {
	g := &generator{state: 2026}
	for _, directives := range append(append([]string{}, crateDirectives...), filterDirectives...) {
		for _, mode := range []string{"new", "try_new"} {
			for _, regex := range []bool{true, false} {
				yield(filterSpec{Op: "filter", Directives: ptr(directives), Mode: mode, Regex: ptr(regex)})
			}
		}
	}
	for range 500 {
		yield(filterSpec{Op: "filter", Directives: ptr(g.directives()), Mode: pick(g, []string{"new", "try_new"}), Regex: ptr(g.chance(75))})
	}
}

func directiveCases(yield func(any)) {
	g := &generator{state: 11}
	for _, text := range crateDirectives {
		yield(directiveRequest{Op: "directive", Text: text})
		for _, part := range strings.Split(text, ",") {
			yield(directiveRequest{Op: "directive", Text: part})
		}
	}
	// Leading whitespace shifts the crate's cuts; before a multi-byte
	// character one falls inside it.
	for _, text := range []string{" info", "  mj=debug", " é=info", "\té=x", " [é]", "é é", "　info"} {
		yield(directiveRequest{Op: "directive", Text: text})
	}
	for range 600 {
		yield(directiveRequest{Op: "directive", Text: g.directive()})
	}
}
