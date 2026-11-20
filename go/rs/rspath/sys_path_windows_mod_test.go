// Port of std 1.99.1 src/sys/path/windows/tests.rs
//
// The prefix parser and Components only read the text, so these run on any
// host. Not ported: verbatim and test_is_absolute_exact, which test
// maybe_verbatim and is_absolute_exact. Those call GetFullPathNameW for the
// file system functions and belong to go/rs/rsfs.

package rspath

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

func TestSysPathWindows_test_parse_next_component(t *testing.T) {
	check := func(path string, verbatim bool, component, rest string) {
		t.Helper()
		gotComponent, gotRest := windowsParseNextComponent(path, verbatim)
		if gotComponent != component || gotRest != rest {
			t.Errorf("parse_next_component(%q, %v) = (%q, %q), expected (%q, %q)",
				path, verbatim, gotComponent, gotRest, component, rest)
		}
	}

	check(`server\share`, true, `server`, `share`)

	check(`server/share`, true, `server/share`, ``)

	check(`server/share`, false, `server`, `share`)

	check(`server\`, false, `server`, ``)

	check(`\server\`, false, ``, `server\`)

	check(`servershare`, false, `servershare`, "")
}

func TestSysPathWindows_test_parse_prefix_verbatim(t *testing.T) {
	prefix := rs.Some[Prefix](&Prefix_VerbatimDisk{F0: 'C'})
	for _, path := range []string{`\\?\C:/windows/system32/notepad.exe`, `\\?\C:\windows\system32\notepad.exe`} {
		if got := windowsParsePrefix(path); !rs.Eq(prefix, got) {
			t.Errorf("parse_prefix(%q) = %v", path, debugPrefix(got))
		}
	}
}

func TestSysPathWindows_test_parse_prefix_verbatim_device(t *testing.T) {
	prefix := rs.Some[Prefix](&Prefix_UNC{F0: "?", F1: "C:"})
	for _, path := range []string{
		`//?/C:/windows/system32/notepad.exe`,
		`//?/C:\windows\system32\notepad.exe`,
		`/\?\C:\windows\system32\notepad.exe`,
		`\\?/C:\windows\system32\notepad.exe`,
	} {
		if got := windowsParsePrefix(path); !rs.Eq(prefix, got) {
			t.Errorf("parse_prefix(%q) = %v", path, debugPrefix(got))
		}
	}
}

func debugPrefix(prefix rs.Option[Prefix]) string {
	if p, ok := prefix.Get(); ok {
		return "Some(" + Windows.prefixDebugString(p) + ")"
	}
	return "None"
}

// See #93586 for more information.
func TestSysPathWindows_test_windows_prefix_components(t *testing.T) {
	path := "C:"
	components := Windows.Components(path)
	drive := components.Next().Expect("drive is expected here")
	if drive.AsOsStr() != "C:" {
		t.Errorf("drive = %q", drive.AsOsStr())
	}
	if !Windows.Eq(components.AsPath(), "") {
		t.Errorf("as_path = %q", components.AsPath())
	}
}

// See #101358.
//
// Note that the exact behavior here may change in the future.
// In which case this test will need to adjusted.
func TestSysPathWindows_broken_unc_path(t *testing.T) {
	check := func(got rs.Option[Component], expected Component) {
		t.Helper()
		if !rs.Eq(got, rs.Some(expected)) {
			t.Errorf("got %v", got)
		}
	}

	components := Windows.Components(`\\foo\\bar\\`)
	check(components.Next(), &Component_RootDir{})
	check(components.Next(), &Component_Normal{F0: "foo"})
	check(components.Next(), &Component_Normal{F0: "bar"})

	components = Windows.Components("//foo//bar//")
	check(components.Next(), &Component_RootDir{})
	check(components.Next(), &Component_Normal{F0: "foo"})
	check(components.Next(), &Component_Normal{F0: "bar"})
}
