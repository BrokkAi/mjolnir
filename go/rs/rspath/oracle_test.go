package rspath

import (
	"encoding/hex"
	"encoding/json"
	"math/rand/v2"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
)

// The differential tests ask the real std::path (go/port/oracle, module
// std_path) what it does with a path and hold this package to the answer.
//
// std::path has one syntax compiled in, so an oracle answers for the platform
// it was built for. Each corpus is therefore recorded on one platform and
// names its flavour in every request, and is replayed against that flavour of
// this package on any host:
//
//	testdata/oracle/std_path_unix.jsonl.gz      recorded on Linux
//	testdata/oracle/std_path_windows.jsonl.gz   to be recorded on Windows
//
// MJ_ORACLE=live and MJ_ORACLE=record talk to the oracle of the host, so they
// cover the host's flavour. The corpus of the other flavour is replayed when
// it is there.

type oracleRequest struct {
	Op     string `json:"op"`
	Flavor string `json:"flavor"`
	P      string `json:"p"`
	Q      string `json:"q,omitempty"`
	Steps  string `json:"steps,omitempty"`
	Pretty bool   `json:"pretty,omitempty"`
	Fp     int    `json:"fp,omitempty"`
	Bp     int    `json:"bp,omitempty"`
	Fq     int    `json:"fq,omitempty"`
	Bq     int    `json:"bq,omitempty"`
}

func flavorName(f Flavor) string {
	if f.resolve() == Windows {
		return "windows"
	}
	return "unix"
}

func flavorNamed(t *testing.T, name string) Flavor {
	t.Helper()
	switch name {
	case "unix":
		return Unix
	case "windows":
		return Windows
	default:
		t.Fatalf("oracle request names no flavour: %q", name)
		return Unix
	}
}

func toHex(s string) string { return hex.EncodeToString([]byte(s)) }

func fromHex(t *testing.T, s string) string {
	t.Helper()
	bytes, err := hex.DecodeString(s)
	if err != nil {
		t.Fatalf("oracle request holds bad hexadecimal %q: %v", s, err)
	}
	return string(bytes)
}

func optionalHex(o rs.Option[string]) any {
	if value, ok := o.Get(); ok {
		return toHex(value)
	}
	return nil
}

func optionalText(o rs.Option[string]) any {
	if value, ok := o.Get(); ok {
		return value
	}
	return nil
}

func hexList(values []string) []any {
	out := make([]any, 0, len(values))
	for _, value := range values {
		out = append(out, toHex(value))
	}
	return out
}

func prefixReply(f Flavor, p Prefix) map[string]any {
	reply := map[string]any{
		"a":        nil,
		"b":        nil,
		"d":        nil,
		"verbatim": p.IsVerbatim(),
		"dbg":      agree(f.prefixDebugString(p), rs.DebugString(prefixDebug{flavor: f, prefix: p})),
	}
	switch p := p.(type) {
	case *Prefix_Verbatim:
		reply["k"], reply["a"] = "Verbatim", toHex(p.F0)
	case *Prefix_VerbatimUNC:
		reply["k"], reply["a"], reply["b"] = "VerbatimUNC", toHex(p.F0), toHex(p.F1)
	case *Prefix_VerbatimDisk:
		reply["k"], reply["d"] = "VerbatimDisk", p.F0
	case *Prefix_DeviceNS:
		reply["k"], reply["a"] = "DeviceNS", toHex(p.F0)
	case *Prefix_UNC:
		reply["k"], reply["a"], reply["b"] = "UNC", toHex(p.F0), toHex(p.F1)
	case *Prefix_Disk:
		reply["k"], reply["d"] = "Disk", p.F0
	}
	return reply
}

func componentReply(f Flavor, c Component) map[string]any {
	reply := map[string]any{
		"v":      nil,
		"prefix": nil,
		"as":     toHex(f.ComponentAsOsStr(c)),
		"dbg":    agree(f.componentDebugString(c), rs.DebugString(componentDebug{flavor: f, component: c})),
	}
	switch c := c.(type) {
	case *Component_Prefix:
		reply["t"] = "Prefix"
		reply["prefix"] = prefixReply(f, c.F0.Kind())
	case *Component_RootDir:
		reply["t"] = "RootDir"
	case *Component_CurDir:
		reply["t"] = "CurDir"
	case *Component_ParentDir:
		reply["t"] = "ParentDir"
	case *Component_Normal:
		reply["t"] = "Normal"
		reply["v"] = toHex(c.F0)
	}
	return reply
}

// agree returns the text when every way the Go code has of producing it
// gives the same text, and otherwise all of them, so that the case differs
// from Rust's answer and is reported.
func agree(texts ...string) string {
	for _, text := range texts[1:] {
		if text != texts[0] {
			return "the Go forms disagree: " + strings.Join(texts, " | ")
		}
	}
	return texts[0]
}

// pathDebug is `format!("{:?}", path)`, which DebugString, the formatter on
// Debug and, under the host's flavour, the formatter on a Key must all give.
func pathDebug(f Flavor, path string) string {
	if f.resolve() == Host {
		return agree(f.DebugString(path), rs.DebugString(f.Debug(path)), rs.DebugString(Key(path)))
	}
	return agree(f.DebugString(path), rs.DebugString(f.Debug(path)))
}

// componentsListPretty is `format!("{:#?}", components.collect::<Vec<_>>())`.
// Under the host's flavour the components print themselves (their FmtDebug
// methods and registrations); under any flavour componentDebug prints them.
// Both must give Rust's text.
func componentsListPretty(f Flavor, path string) string {
	var wrapped []componentDebug
	for c := range f.Components(path).All() {
		wrapped = append(wrapped, componentDebug{flavor: f, component: c})
	}
	text := rs.Format("{:#?}", wrapped)
	if f.resolve() == Host {
		components := f.Components(path).Collect()
		if components == nil {
			components = []Component{}
		}
		return agree(text, rs.Format("{:#?}", components))
	}
	return text
}

func optionalComponentReply(f Flavor, c rs.Option[Component]) any {
	if value, ok := c.Get(); ok {
		return componentReply(f, value)
	}
	return nil
}

func inspectReply(f Flavor, path string, pretty bool) any {
	components := make([]any, 0)
	for c := range f.Components(path).All() {
		components = append(components, componentReply(f, c))
	}
	componentsRev := make([]any, 0)
	for c := range f.Components(path).Rev() {
		componentsRev = append(componentsRev, f.componentDebugString(c))
	}
	var iterRev []string
	for part := range f.Iter(path).Rev() {
		iterRev = append(iterRev, part)
	}
	popped := path
	didPop := f.Pop(&popped)
	var intoString any
	if value, err := f.OsStringIntoString(path); err == nil {
		intoString = value
	}
	reply := map[string]any{
		"components":       components,
		"components_rev":   componentsRev,
		"iter":             hexList(f.Iter(path).Collect()),
		"iter_rev":         hexList(iterRev),
		"ancestors":        hexList(f.Ancestors(path).Collect()),
		"count":            f.Components(path).Count(),
		"parent":           optionalHex(f.Parent(path)),
		"file_name":        optionalHex(f.FileName(path)),
		"file_stem":        optionalHex(f.FileStem(path)),
		"file_prefix":      optionalHex(f.FilePrefix(path)),
		"extension":        optionalHex(f.Extension(path)),
		"is_absolute":      f.IsAbsolute(path),
		"is_relative":      f.IsRelative(path),
		"has_root":         f.HasRoot(path),
		"is_empty":         f.IsEmpty(path),
		"to_str":           optionalText(f.ToStr(path)),
		"lossy":            f.ToStringLossy(path),
		"display":          f.Display(path),
		"debug":            pathDebug(f, path),
		"into_string":      intoString,
		"components_debug": agree(f.Components(path).DebugString(), rs.DebugString(f.Components(path))),
		"iter_debug":       agree(f.Iter(path).DebugString(), rs.DebugString(f.Iter(path))),
		"pop":              []any{toHex(popped), didPop},
		"collected":        toHex(f.PathBufFromComponents(f.Components(path).Collect())),
		"self_eq":          f.Eq(path, path),
	}
	if pretty {
		// The pretty forms are the formatter's alone.
		reply["components_pretty"] = rs.Format("{:#?}", f.Components(path))
		reply["components_list_pretty"] = componentsListPretty(f, path)
		reply["iter_pretty"] = rs.Format("{:#?}", f.Iter(path))
	}
	return reply
}

// keyed is a type that derives PartialEq and Ord with a PathBuf field: rs.Eq
// and rs.Cmp reach the field's Equal and Compare.
type keyed struct {
	Name  string `rs:"name"`
	Path  Key    `rs:"path"`
	Order int    `rs:"order"`
}

// pairEq is `path == other`, which every way the Go code has of asking must
// answer alike: Eq, HashKey equality and, under the host's flavour, Key
// directly, through rs.Eq and as a field of a struct.
func pairEq(f Flavor, path, other string) any {
	eq := f.Eq(path, other)
	answers := map[string]bool{"HashKey": f.HashKey(path) == f.HashKey(other)}
	if f.resolve() == Host {
		answers["Key.Equal"] = Key(path).Equal(Key(other))
		answers["rs.Eq(Key)"] = rs.Eq(Key(path), Key(other))
		answers["Key.HashKey"] = Key(path).HashKey() == Key(other).HashKey()
		answers["rs.Eq(struct)"] = rs.Eq(keyed{Name: "x", Path: Key(path)}, keyed{Name: "x", Path: Key(other)})
	}
	for way, answer := range answers {
		if answer != eq {
			return "Eq says " + strconv.FormatBool(eq) + " and " + way + " says " + strconv.FormatBool(answer)
		}
	}
	return eq
}

// pairCmp is `path.cmp(&other)`, asked as pairEq asks equality.
func pairCmp(f Flavor, path, other string) any {
	order := f.Cmp(path, other)
	answers := map[string]int{}
	if f.resolve() == Host {
		answers["Key.Compare"] = Key(path).Compare(Key(other))
		answers["rs.Cmp(Key)"] = rs.Cmp(Key(path), Key(other))
		// Equal paths leave the order to the next field, which is greater
		// on the left.
		structOrder := rs.Cmp(keyed{Name: "x", Path: Key(path), Order: 2}, keyed{Name: "x", Path: Key(other), Order: 1})
		if order == 0 {
			structOrder--
		}
		answers["rs.Cmp(struct)"] = structOrder
	}
	for way, answer := range answers {
		if answer != order {
			return "Cmp says " + strconv.Itoa(order) + " and " + way + " says " + strconv.Itoa(answer)
		}
	}
	return order
}

func pairReply(f Flavor, path, other string) any {
	var stripOk, stripErr any
	if rest, err := f.StripPrefix(path, other); err == nil {
		stripOk = toHex(rest)
	} else {
		stripErr = err.Error()
	}

	setFileName := path
	f.SetFileName(&setFileName, other)

	left, right := f.Components(path), f.Components(other)
	return map[string]any{
		"join":             toHex(f.Join(path, other)),
		"starts_with":      f.StartsWith(path, other),
		"ends_with":        f.EndsWith(path, other),
		"strip_prefix":     stripOk,
		"strip_prefix_err": stripErr,
		"eq":               pairEq(f, path, other),
		"cmp":              pairCmp(f, path, other),
		// HashKey is the same exactly when the paths are equal, so Rust's
		// "equal paths hash alike" holds for it by the comparison of "eq".
		"eq_implies_hash_eq": !f.Eq(path, other) || f.HashKey(path) == f.HashKey(other),
		"components_eq":      left.Equal(right),
		"components_cmp":     left.Compare(right),
		"with_file_name":     toHex(f.WithFileName(path, other)),
		"set_file_name":      toHex(setFileName),
		"set_extension": oracle.Catch(func() any {
			buf := path
			changed := f.SetExtension(&buf, other)
			return []any{toHex(buf), changed}
		}),
		"add_extension": oracle.Catch(func() any {
			buf := path
			changed := f.AddExtension(&buf, other)
			return []any{toHex(buf), changed}
		}),
		"with_extension":       oracle.Catch(func() any { return toHex(f.WithExtension(path, other)) }),
		"with_added_extension": oracle.Catch(func() any { return toHex(f.WithAddedExtension(path, other)) }),
	}
}

func walkReply(t *testing.T, f Flavor, path, steps string) any {
	t.Helper()
	components := f.Components(path)
	out := make([]any, 0, len(steps))
	for _, step := range steps {
		switch step {
		case 'F':
			out = append(out, optionalComponentReply(f, components.Next()))
		case 'B':
			out = append(out, optionalComponentReply(f, components.NextBack()))
		case 'P':
			out = append(out, toHex(components.AsPath()))
		case 'D':
			out = append(out, components.DebugString())
		default:
			t.Fatalf("unknown walk step %q", step)
		}
	}
	return out
}

func advanced(f Flavor, path string, front, back int) *PathComponents {
	components := f.Components(path)
	for range front {
		components.Next()
	}
	for range back {
		components.NextBack()
	}
	return components
}

func cmpComponentsReply(f Flavor, request oracleRequest, p, q string) any {
	left := advanced(f, p, request.Fp, request.Bp)
	right := advanced(f, q, request.Fq, request.Bq)
	return map[string]any{
		"eq":    left.Equal(right),
		"cmp":   left.Clone().Compare(right.Clone()),
		"left":  toHex(left.AsPath()),
		"right": toHex(right.AsPath()),
	}
}

func osStrReply(f Flavor, first, second string) any {
	pushed := first
	f.OsStringPush(&pushed, second)
	var intoString, intoStringErr any
	if value, err := f.OsStringIntoString(first); err == nil {
		intoString = value
	} else {
		intoStringErr = f.DebugString(err.(*IntoStringError).OsString)
	}
	var wide any
	if f.resolve() == Windows {
		units := OsStrEncodeWide(first)
		if units == nil {
			units = []uint16{}
		}
		wide = map[string]any{"units": units, "back": toHex(OsStringFromWide(units))}
	}
	return map[string]any{
		"to_str":               optionalText(f.OsStrToStr(first)),
		"lossy":                f.OsStrToStringLossy(first),
		"display":              f.OsStrDisplay(first),
		"debug":                f.DebugString(first),
		"into_string":          intoString,
		"into_string_err":      intoStringErr,
		"len":                  OsStrLen(first),
		"is_empty":             OsStrIsEmpty(first),
		"is_ascii":             OsStrIsAscii(first),
		"lower":                toHex(OsStrToAsciiLowercase(first)),
		"upper":                toHex(OsStrToAsciiUppercase(first)),
		"push":                 toHex(pushed),
		"eq":                   first == second,
		"cmp":                  strings.Compare(first, second),
		"eq_ignore_ascii_case": OsStrEqIgnoreAsciiCase(first, second),
		"join":                 toHex(f.OsStrJoin([]string{first, second, first}, second)),
		"wide":                 wide,
	}
}

// debugClassesReply says how the Debug form writes every char: as itself (0),
// as \u{..} (1) or with a backslash escape (2, listed in "specials"). Runs of
// code points of one class are given as [first, last, class]. It holds the
// Unicode tables of this package to the ones Rust was built with.
func debugClassesReply(f Flavor) any {
	ranges := make([][3]uint32, 0, 4096)
	specials := map[string]string{}
	for code := uint32(0); code <= 0x10FFFF; code++ {
		if code >= 0xD800 && code <= 0xDFFF {
			continue
		}
		text := string(rune(code))
		shown := f.DebugString(text)
		var class uint32
		switch shown {
		case `"` + text + `"`:
			class = 0
		case `"\u{` + strconv.FormatUint(uint64(code), 16) + `}"`:
			class = 1
		default:
			class = 2
			specials[strconv.FormatUint(uint64(code), 10)] = shown
		}
		if last := len(ranges) - 1; last >= 0 && ranges[last][2] == class && ranges[last][1]+1 == code {
			ranges[last][1] = code
		} else {
			ranges = append(ranges, [3]uint32{code, code, class})
		}
	}
	return map[string]any{"ranges": ranges, "specials": specials}
}

func answerPathRequest(t *testing.T) func(json.RawMessage) any {
	return func(raw json.RawMessage) any {
		var request oracleRequest
		if err := json.Unmarshal(raw, &request); err != nil {
			t.Fatalf("oracle request does not decode: %v\n%s", err, raw)
		}
		f := flavorNamed(t, request.Flavor)
		p := fromHex(t, request.P)
		q := fromHex(t, request.Q)
		switch request.Op {
		case "inspect":
			return inspectReply(f, p, request.Pretty)
		case "pair":
			return pairReply(f, p, q)
		case "walk":
			return walkReply(t, f, p, request.Steps)
		case "cmp_components":
			return cmpComponentsReply(f, request, p, q)
		case "os_str":
			return osStrReply(f, p, q)
		case "debug_classes":
			return debugClassesReply(f)
		default:
			t.Fatalf("unknown oracle operation %q", request.Op)
			return nil
		}
	}
}

// unixEdgePaths are the fixed inputs of the Unix corpus.
var unixEdgePaths = []string{
	"", "/", "//", "///", ".", "..", "...", "./", "../", "/.", "/..", "./.", "../..", "././", ".//",
	"a", "a/", "a//", "/a", "//a", "///a", "a/b", "a//b", "a/./b", "a/../b", "a/b/", "a/b/.", "a/b/..",
	"./a", "./a/b", "../a", "/a/b/c", "/a/./b/../c/", "a/.", "a/..", "/./a", "/../a", ".a", "..a", "a.",
	"a..", ".a.", "..a..", "foo.txt", "foo.tar.gz", ".hidden", ".hidden.txt", "..hidden", "foo.", "foo..",
	"foo/.hidden", "foo/bar.txt/", "foo/bar.txt/.", "foo/bar.txt//.//", "/foo.ext////", "foo/..txt",
	"..txt", "..txt/", "a/..b", "a/.b.c/", ".x.y.z", "..x.y.z", " ", " / ", "a b/c d", "a\\b", "\\",
	"\\\\server\\share", "C:", "C:\\foo", "c:/foo", "//server/share", "\\\\?\\C:\\x", "a\nb", "a\tb",
	"a\x00b", "\x7f", "tab\there", "quote\"s", "single'quote", "é", "日本語/ファイル.txt", "/tmp/❤/🚀/file.txt",
	"e\u0301", "\u0301", "\u200b", "\ufeff", "\u00a0", "\u0085", "\U000e0001", "\ue000", "\uff9e",
	"\xff", "\xff/\xfe", "a\xffb", "\xc3", "\xc3\xa9", "\xe2\x82", "\xe2\x82\xac", "\xf0\x9f", "\xf0\x9f\x9a",
	"\xf0\x9f\x9a\x80", "\xed\xa0\x80", "\xed\xb0\x80", "\xed\xa0\x80\xed\xb0\x80", "\xc0\xaf", "\xf5",
	"a/\xff.\xfe", "\xff.txt", "x.\xff", "/\xff", "dir/\xe2\x82/file", strings.Repeat("a/", 34) + "b",
	strings.Repeat("long", 40), "/" + strings.Repeat("x", 70) + "." + strings.Repeat("y", 70),
	strings.Repeat("/", 20), strings.Repeat("../", 12), strings.Repeat("./", 12) + "a",
}

// windowsEdgePaths are the fixed inputs of the Windows corpus. All are WTF-8.
var windowsEdgePaths = []string{
	"", `\`, "/", `\\`, "//", `\\\`, ".", "..", `.\`, "./", `..\`, `\.`, `\..`, `.\.`, "a", `a\`, "a/",
	`a\\`, `\a`, "/a", `a\b`, "a/b", `a\/b`, `a/\b`, `a\.\b`, `a\..\b`, `a\b\`, `a\b\.`, `.\a`, `..\a`,
	"C:", "c:", "C:.", "C:..", `C:\`, "C:/", `C:\\`, "C:a", `C:a\b`, `C:\a\b`, "c:/a/b", `C:.\a`, `C:..\a`,
	`C:\.`, `C:\..`, "1:", "1:a", "é:", ":", "a:b", "C", "CC:", `C:\foo.txt`, `C:foo.txt`, "Z:", `z:\`,
	`\\server`, `\\server\`, `\\server\share`, `\\server\share\`, `\\server\share\a`, `\\server\share\a\b\`,
	`//server/share`, `//server/share/a`, `\/server\share`, `/\server/share`, `\\server\\share`,
	`\\\server\share`, `\\server\share\..`, `\\server\share\.`, `\\server/share\a`, `\\.\`, `\\.\COM1`,
	`\\.\COM1\`, `\\.\pipe\name`, `//./COM1`, `\\.\C:\a`, `\\.`, `\\..\a\b`, `\\?\`, `\\?`, `\\?\C:`,
	`\\?\C:\`, `\\?\C:\a`, `\\?\C:\a\b\`, `\\?\C:a`, `\\?\C:/a`, `\\?\C:/a/b`, `\\?\c:\a`, `\\?\C:\.\a`,
	`\\?\C:\..\a`, `\\?\C:\a\.`, `\\?\C:\a\..`, `\\?\C:\a/b`, `\\?\C:\\a`, `\\?\a`, `\\?\a\`, `\\?\a\b`,
	`\\?\a/b\c`, `\\?\.`, `\\?\..`, `\\?\UNC`, `\\?\UNC\`, `\\?\UNC\server`, `\\?\UNC\server\`,
	`\\?\UNC\server\share`, `\\?\UNC\server\share\`, `\\?\UNC\server\share\a`, `\\?\UNC/server\share`,
	`\\?\UNC\server/share\a`, `\\?\UN`, `\\?\unc\server\share`, `//?/C:/a`, `//?/UNC/server/share`,
	`/\?\C:\a`, `\\?/C:\a`, `\??\C:\a`, "foo.txt", "foo.tar.gz", ".hidden", "foo.", "..txt", `a\..b`,
	`dir\file.txt\`, `dir\file.txt\.`, `C:\foo.ext\\\\`, "a b", `a b\c d`, "COM1", "NUL", "con.txt",
	"a\nb", "quote\"s", "é", `日本語\ファイル.txt`, `C:\tmp\❤\🚀\file.txt`, "e\u0301", "\u200b",
	"\xed\xa0\x80", "\xed\xb0\x80", "a\xed\xa0\x80", "\xed\xb0\x80a", "\xed\xa0\x80\\\xed\xb0\x80",
	"\xed\xa0\x80.\xed\xb0\x80", "C:\\\xed\xa0\x80\\x.\xed\xbf\xbf", "\xed\xa0\x80\xed\xa0\x80",
	"\xed\xb0\x80\xed\xa0\x80", "\\\\?\\\xed\xa0\x80\\a", strings.Repeat(`a\`, 34) + "b",
	strings.Repeat("long", 40), `C:\` + strings.Repeat("x", 70) + "." + strings.Repeat("y", 70),
	strings.Repeat(`\`, 20), strings.Repeat(`..\`, 12), strings.Repeat("/", 9),
}

var unixPieces = []string{
	"/", "/", "/", "//", ".", "..", "a", "b", "foo", "bar", "foo.txt", ".hidden", "x.tar.gz", "..x",
	"a.", "...", " ", "\\", "\n", "é", "日本", "\xff", "\xc3", "\xe2\x82", "\xed\xa0\x80", "\x00", "C:",
	"?", "*", "\"", "\t", "\x7f", "\u0301", "\u200b", "🚀", "./", "../", "/.", "/..",
}

var windowsPieces = []string{
	`\`, `\`, "/", `\\`, "//", "C:", "c:", "z:", "1:", `\\?\`, `\\.\`, "//?/", "UNC", `UNC\`, "?", ".",
	"..", "server", "share", "foo", "bar.txt", ".hidden", "a.", "COM1", " ", "é", "\xed\xa0\x80",
	"\xed\xb0\x80", "🚀", ":", "*", `.\`, `..\`, `\.`, `\..`, "a", "b", "x.tar.gz", "\n", "\u0301",
}

var extensionNames = []string{
	"", "txt", "tar.gz", ".", "..", ".x", "x.", "a b", "é", "🚀", "/", "a/b", `\`, `a\b`, "foo", "bar",
	".hidden", "x.y.z", "c:", "C:x", `\\?\`,
}

// joinSurrogatePairs makes a random Windows string WTF-8: a lead surrogate
// directly followed by a trail surrogate is the one code point they encode.
func joinSurrogatePairs(s string) string {
	for i := 0; i+6 <= len(s); i++ {
		if s[i] == 0xED && s[i+1] >= 0xA0 && s[i+1] <= 0xAF && s[i+3] == 0xED && s[i+4] >= 0xB0 && s[i+4] <= 0xBF {
			return joinSurrogatePairs(wtf8PushWtf8(s[:i+3], s[i+3:]))
		}
	}
	return s
}

type pathGenerator struct {
	flavor Flavor
	rng    *rand.Rand
}

func newPathGenerator(f Flavor) *pathGenerator {
	seed := uint64(0x6d6a6f6c6e6972)
	if f.resolve() == Windows {
		seed++
	}
	return &pathGenerator{flavor: f.resolve(), rng: rand.New(rand.NewPCG(seed, 0x7273706174680a))}
}

func (g *pathGenerator) pieces() []string {
	if g.flavor == Windows {
		return windowsPieces
	}
	return unixPieces
}

func (g *pathGenerator) edges() []string {
	if g.flavor == Windows {
		return windowsEdgePaths
	}
	return unixEdgePaths
}

func (g *pathGenerator) fix(s string) string {
	if g.flavor == Windows {
		return joinSurrogatePairs(s)
	}
	return s
}

// path builds a random path out of pieces, separators included, so that
// repeated and trailing separators, dots and prefixes turn up everywhere.
func (g *pathGenerator) path() string {
	pieces := g.pieces()
	var b strings.Builder
	for range g.rng.IntN(9) {
		b.WriteString(pieces[g.rng.IntN(len(pieces))])
	}
	return g.fix(b.String())
}

// related builds a path that shares components with path: part of it,
// possibly spelled differently, so that prefix, suffix and equality tests
// have something to find.
func (g *pathGenerator) related(path string) string {
	parts := g.flavor.Iter(path).Collect()
	separator := g.flavor.MainSeparatorStr()
	if g.flavor == Windows && g.rng.IntN(3) == 0 {
		separator = "/"
	}
	extra := []string{separator, separator + "." + separator, separator + separator}
	switch g.rng.IntN(6) {
	case 0: // a leading run of the components
		parts = parts[:g.rng.IntN(len(parts)+1)]
	case 1: // a trailing run
		parts = parts[g.rng.IntN(len(parts)+1):]
	case 2: // the same path, spelled with noise
	case 3: // one component changed
		if len(parts) > 0 {
			parts[g.rng.IntN(len(parts))] = g.pieces()[g.rng.IntN(len(g.pieces()))]
		}
	case 4:
		return g.path()
	default:
		return g.fix(path + extra[g.rng.IntN(len(extra))])
	}
	var b strings.Builder
	for index, part := range parts {
		if index > 0 && !g.flavor.isSepByte(part[0]) && b.Len() > 0 {
			last := b.String()[b.Len()-1]
			if !g.flavor.isSepByte(last) && !(g.flavor == Windows && last == ':') {
				b.WriteString(extra[g.rng.IntN(len(extra))])
			}
		}
		b.WriteString(part)
	}
	return g.fix(b.String())
}

func (g *pathGenerator) requests(yield func(any)) {
	flavor := flavorName(g.flavor)
	edges := g.edges()
	send := func(request oracleRequest) {
		request.Flavor = flavor
		request.P = toHex(request.P)
		request.Q = toHex(request.Q)
		yield(request)
	}

	// Fixed cases first.
	send(oracleRequest{Op: "debug_classes"})
	for _, path := range edges {
		send(oracleRequest{Op: "inspect", P: path, Pretty: true})
	}
	for index, path := range edges {
		for _, step := range []int{1, 7, 13} {
			send(oracleRequest{Op: "pair", P: path, Q: edges[(index+step)%len(edges)]})
		}
		send(oracleRequest{Op: "pair", P: path, Q: path})
		send(oracleRequest{Op: "pair", P: path, Q: extensionNames[index%len(extensionNames)]})
		send(oracleRequest{Op: "os_str", P: path, Q: edges[(index+3)%len(edges)]})
		send(oracleRequest{Op: "walk", P: path, Steps: "PDFPBPFPBPDFB"})
		send(oracleRequest{Op: "walk", P: path, Steps: "BPBPBPFPFPF"})
	}
	// Pairs that byte order and component order put the other way round,
	// and spellings of one path.
	ordering := [][2]string{
		{"a/b", "a-b"}, {"a/b", "a.b"}, {"a/b", "a b"}, {"a/b", "a!b"}, {"a/b/c", "a/bc"}, {"a/b", "a/b/c"},
		{"a//b", "a/b"}, {"a/./b", "a/b"}, {"a/b/", "a/b"}, {"a/b/.", "a/b"}, {"./a", "a"}, {"/a", "a"},
		{"/a/b", "/a-b"}, {"a/..", "a"}, {"../a", "a"}, {"", "."}, {"", "/"}, {"a\\b", "a/b"},
	}
	if g.flavor == Windows {
		ordering = append(ordering, [][2]string{
			{`a\b`, "a-b"}, {`a\\b`, `a/b`}, {`a\.\b`, "a/b"}, {`C:\a`, `c:/a`}, {`C:\a`, `C:a`}, {`\\?\C:\a`, `C:\a`},
			{`\\server\share\a`, `//server/share/a`}, {`\\server\share`, `\\server\share\`}, {`C:`, `D:`},
			{`\\?\UNC\s\t`, `\\s\t`}, {`\\.\COM1`, `\\?\COM1`}, {`\a`, `C:\a`},
		}...)
	}
	for _, pair := range ordering {
		send(oracleRequest{Op: "pair", P: pair[0], Q: pair[1]})
		send(oracleRequest{Op: "pair", P: pair[1], Q: pair[0]})
	}

	if g.flavor == Windows {
		// OsString::push joins a lead and a trail surrogate.
		for _, pair := range [][2]string{
			{"\xed\xa0\xbd", "\xed\xb2\xa9"}, {"a\xed\xa0\x80", "\xed\xb0\x80b"}, {"\xed\xa0\x80", "\xed\xa0\x80"},
			{"\xed\xb0\x80", "\xed\xa0\x80"}, {"\xed\xa0\x80", "a"}, {"a", "\xed\xb0\x80"},
		} {
			send(oracleRequest{Op: "os_str", P: pair[0], Q: pair[1]})
			send(oracleRequest{Op: "pair", P: pair[0], Q: pair[1]})
		}
	}

	// Then seeded random ones.
	for range 900 {
		send(oracleRequest{Op: "inspect", P: g.path()})
	}
	for range 2000 {
		path := g.path()
		send(oracleRequest{Op: "pair", P: path, Q: g.related(path)})
	}
	for range 400 {
		send(oracleRequest{Op: "pair", P: g.path(), Q: extensionNames[g.rng.IntN(len(extensionNames))]})
	}
	for range 700 {
		steps := make([]byte, 4+g.rng.IntN(10))
		for index := range steps {
			steps[index] = "FFBBPD"[g.rng.IntN(6)]
		}
		send(oracleRequest{Op: "walk", P: g.path(), Steps: string(steps)})
	}
	for range 700 {
		path := g.path()
		send(oracleRequest{
			Op: "cmp_components", P: path, Q: g.related(path),
			Fp: g.rng.IntN(3), Bp: g.rng.IntN(3), Fq: g.rng.IntN(3), Bq: g.rng.IntN(3),
		})
	}
	for range 300 {
		first := g.path()
		send(oracleRequest{Op: "os_str", P: first, Q: g.related(first)})
	}
}

// diffAgainstRust holds one flavour to its corpus. For the host's flavour
// that is the usual oracle check, which MJ_ORACLE=live and MJ_ORACLE=record
// extend to the oracle itself. The other flavour has no oracle on this host:
// its corpus is replayed when one has been recorded.
func diffAgainstRust(t *testing.T, f Flavor) {
	corpus := "std_path_" + flavorName(f)
	if f.resolve() == Host {
		oracle.Check(t, "std_path", corpus, newPathGenerator(f).requests, answerPathRequest(t))
		return
	}
	if !corpusRecorded(corpus) {
		t.Skipf("no corpus is recorded for the %s flavour: std::path only speaks the syntax of the platform it is "+
			"built for, so run MJ_ORACLE=record on a %s host", flavorName(f), flavorName(f))
	}
	if os.Getenv("MJ_ORACLE") == "record" {
		t.Skipf("the %s corpus is recorded on a %s host", flavorName(f), flavorName(f))
	}
	oracle.Check(t, "std_path", corpus, nil, answerPathRequest(t))
}

// corpusRecorded reports whether the oracle package has a corpus to replay:
// the compressed file it writes, or the plain one it wrote at first.
func corpusRecorded(corpus string) bool {
	for _, suffix := range []string{".jsonl.gz", ".jsonl"} {
		if _, err := os.Stat(filepath.Join("testdata", "oracle", corpus+suffix)); err == nil {
			return true
		}
	}
	return false
}

func TestDiffAgainstRust(t *testing.T) {
	diffAgainstRust(t, Host)
}

// TestDiffAgainstRustOtherFlavor replays the corpus recorded on the other
// platform against that flavour, which this host cannot ask an oracle about.
func TestDiffAgainstRustOtherFlavor(t *testing.T) {
	if Host == Windows {
		diffAgainstRust(t, Unix)
	} else {
		diffAgainstRust(t, Windows)
	}
}
