package rspath

import (
	"slices"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// The Windows flavour has no oracle on a Unix host. Where a path has no
// prefix, though, Windows reads it with the state machine Unix uses and one
// more separator, so what Rust says about `a/b//c/.` on Unix it says about
// `a\b/\c\.` on Windows, with `\` where it wrote `/`. The Unix flavour is
// held to Rust by the recorded corpus; this holds the Windows flavour to the
// Unix flavour over the same generated paths, each respelled with a random
// mix of the two Windows separators.
//
// What only Windows has (drive, UNC, device and verbatim prefixes) is covered
// by the tables ported from Rust's own tests.

// respell rewrites a Unix path that holds no backslash with a mix of `/` and
// `\`, and reports whether Windows reads the result as having no prefix.
func respell(g *pathGenerator, path string) (string, bool) {
	if strings.Contains(path, `\`) {
		return "", false
	}
	bytes := []byte(path)
	for index, b := range bytes {
		if b == '/' && g.rng.IntN(2) == 0 {
			bytes[index] = '\\'
		}
	}
	mixed := string(bytes)
	if Windows.parsePrefix(mixed).IsSome() {
		return "", false
	}
	return mixed, true
}

// slashes undoes respell on a result: every backslash in it is a separator.
func slashes(s string) string {
	return strings.ReplaceAll(s, `\`, "/")
}

func slashesOption(o rs.Option[string]) rs.Option[string] {
	return rs.OptionMap(o, slashes)
}

func slashesAll(values []string) []string {
	out := make([]string, 0, len(values))
	for _, value := range values {
		out = append(out, slashes(value))
	}
	return out
}

// componentNames writes components without their flavour: RootDir is not
// spelled, and a name is its bytes.
func componentNames(components []Component) []string {
	out := make([]string, 0, len(components))
	for _, component := range components {
		out = append(out, Unix.componentDebugString(component))
	}
	return out
}

func optionalName(component rs.Option[Component]) string {
	if c, ok := component.Get(); ok {
		return Unix.componentDebugString(c)
	}
	return "None"
}

const walkSteps = "FBPFPBBPFFPB"

// plainExtensions hold no separator of either flavour.
var plainExtensions = []string{"", "txt", "tar.gz", ".", "..", ".x", "x.", "a b", "é", "🚀", "x.y.z"}

func TestWindowsFlavorAgreesWithUnixOnPrefixlessPaths(t *testing.T) {
	g := newPathGenerator(Unix)
	compared := 0
	fail := func(what, unixPath, windowsPath string, unix, windows any) {
		t.Helper()
		t.Errorf("%s differs\nunix    %q: %#v\nwindows %q: %#v", what, unixPath, unix, windowsPath, windows)
	}

	paths := slices.Clone(unixEdgePaths)
	for range 4000 {
		paths = append(paths, g.path())
	}
	for _, unixPath := range paths {
		windowsPath, ok := respell(g, unixPath)
		if !ok {
			continue
		}
		compared++

		if u, w := componentNames(Unix.Components(unixPath).Collect()), componentNames(Windows.Components(windowsPath).Collect()); !slices.Equal(u, w) {
			fail("components", unixPath, windowsPath, u, w)
		}
		var unixRev, windowsRev []Component
		for c := range Unix.Components(unixPath).Rev() {
			unixRev = append(unixRev, c)
		}
		for c := range Windows.Components(windowsPath).Rev() {
			windowsRev = append(windowsRev, c)
		}
		if u, w := componentNames(unixRev), componentNames(windowsRev); !slices.Equal(u, w) {
			fail("components().rev()", unixPath, windowsPath, u, w)
		}
		if u, w := Unix.Iter(unixPath).Collect(), slashesAll(Windows.Iter(windowsPath).Collect()); !slices.Equal(u, w) {
			fail("iter", unixPath, windowsPath, u, w)
		}
		if u, w := Unix.Ancestors(unixPath).Collect(), slashesAll(Windows.Ancestors(windowsPath).Collect()); !slices.Equal(u, w) {
			fail("ancestors", unixPath, windowsPath, u, w)
		}
		for name, method := range map[string]func(Flavor, string) rs.Option[string]{
			"parent": Flavor.Parent, "file_name": Flavor.FileName, "file_stem": Flavor.FileStem,
			"file_prefix": Flavor.FilePrefix, "extension": Flavor.Extension,
		} {
			if u, w := method(Unix, unixPath), slashesOption(method(Windows, windowsPath)); u != w {
				fail(name, unixPath, windowsPath, u, w)
			}
		}
		if u, w := Unix.HasRoot(unixPath), Windows.HasRoot(windowsPath); u != w {
			fail("has_root", unixPath, windowsPath, u, w)
		}
		// Without a prefix nothing is absolute on Windows.
		if Windows.IsAbsolute(windowsPath) {
			fail("is_absolute", unixPath, windowsPath, false, true)
		}
		unixPopped, windowsPopped := unixPath, windowsPath
		if u, w := Unix.Pop(&unixPopped), Windows.Pop(&windowsPopped); u != w || unixPopped != slashes(windowsPopped) {
			fail("pop", unixPath, windowsPath, unixPopped, windowsPopped)
		}
		if u, w := Unix.TrimTrailingSep(unixPath), slashes(Windows.TrimTrailingSep(windowsPath)); u != w {
			fail("trim_trailing_sep", unixPath, windowsPath, u, w)
		}
		// normalize_lexically pushes the components one by one, and on
		// Windows pushing a component that begins like a drive (`C:`)
		// replaces what came before it.
		if !strings.Contains(unixPath, ":") {
			unixNormal, unixErr := Unix.NormalizeLexically(unixPath)
			windowsNormal, windowsErr := Windows.NormalizeLexically(windowsPath)
			if (unixErr == nil) != (windowsErr == nil) || unixNormal != slashes(windowsNormal) {
				fail("normalize_lexically", unixPath, windowsPath, unixNormal, windowsNormal)
			}
		}

		// A walk from both ends, with the path left after some of the steps.
		unixWalk, windowsWalk := Unix.Components(unixPath), Windows.Components(windowsPath)
		for index, step := range walkSteps {
			var u, w string
			switch step {
			case 'F':
				u, w = optionalName(unixWalk.Next()), optionalName(windowsWalk.Next())
			case 'B':
				u, w = optionalName(unixWalk.NextBack()), optionalName(windowsWalk.NextBack())
			default:
				u, w = unixWalk.AsPath(), slashes(windowsWalk.AsPath())
			}
			if u != w {
				fail("step "+walkSteps[:index+1]+" of a walk", unixPath, windowsPath, u, w)
				break
			}
		}

		// The extension, replaced and added.
		extension := plainExtensions[compared%len(plainExtensions)]
		unixBuf, windowsBuf := unixPath, windowsPath
		if u, w := Unix.SetExtension(&unixBuf, extension), Windows.SetExtension(&windowsBuf, extension); u != w || unixBuf != slashes(windowsBuf) {
			fail("set_extension "+extension, unixPath, windowsPath, unixBuf, windowsBuf)
		}
		unixBuf, windowsBuf = unixPath, windowsPath
		if u, w := Unix.AddExtension(&unixBuf, extension), Windows.AddExtension(&windowsBuf, extension); u != w || unixBuf != slashes(windowsBuf) {
			fail("add_extension "+extension, unixPath, windowsPath, unixBuf, windowsBuf)
		}
		if u, w := Unix.WithExtension(unixPath, extension), slashes(Windows.WithExtension(windowsPath, extension)); u != w {
			fail("with_extension "+extension, unixPath, windowsPath, u, w)
		}

		// Everything that takes two paths.
		other := g.related(unixPath)
		windowsOther, ok := respell(g, other)
		if !ok {
			continue
		}
		pairFail := func(what string, unix, windows any) {
			t.Helper()
			t.Errorf("%s differs\nunix    %q, %q: %#v\nwindows %q, %q: %#v",
				what, unixPath, other, unix, windowsPath, windowsOther, windows)
		}
		if u, w := Unix.Join(unixPath, other), slashes(Windows.Join(windowsPath, windowsOther)); u != w {
			pairFail("join", u, w)
		}
		if u, w := Unix.StartsWith(unixPath, other), Windows.StartsWith(windowsPath, windowsOther); u != w {
			pairFail("starts_with", u, w)
		}
		if u, w := Unix.EndsWith(unixPath, other), Windows.EndsWith(windowsPath, windowsOther); u != w {
			pairFail("ends_with", u, w)
		}
		unixRest, unixErr := Unix.StripPrefix(unixPath, other)
		windowsRest, windowsErr := Windows.StripPrefix(windowsPath, windowsOther)
		if (unixErr == nil) != (windowsErr == nil) || unixRest != slashes(windowsRest) {
			pairFail("strip_prefix", unixRest, windowsRest)
		}
		if u, w := Unix.Eq(unixPath, other), Windows.Eq(windowsPath, windowsOther); u != w {
			pairFail("eq", u, w)
		}
		if u, w := Unix.Cmp(unixPath, other), Windows.Cmp(windowsPath, windowsOther); u != w {
			pairFail("cmp", u, w)
		}
		if u, w := Unix.HashKey(unixPath) == Unix.HashKey(other), Windows.HashKey(windowsPath) == Windows.HashKey(windowsOther); u != w {
			pairFail("hash key equality", u, w)
		}
		if u, w := Unix.WithFileName(unixPath, other), slashes(Windows.WithFileName(windowsPath, windowsOther)); u != w {
			pairFail("with_file_name", u, w)
		}
	}
	if compared < 2000 {
		t.Fatalf("only %d paths were comparable", compared)
	}
}
