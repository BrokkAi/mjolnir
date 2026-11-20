// Port of std 1.99.1 tests/env.rs
//
// Rust guards the split_paths and join_paths tests with cfg(unix) and
// cfg(windows). This package holds both platforms' rules, so each runs here
// under its own rules on any host. split_paths_uefi is not ported: there is
// no UEFI target.

package rsenv

import (
	"slices"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rspath"
)

func TestItEnv_test_self_exe_path(t *testing.T) {
	path, err := CurrentExe()
	if err != nil {
		t.Fatalf("current_exe: %v", err)
	}

	// Hard to test this function
	if !rspath.IsAbsolute(path) {
		t.Errorf("current_exe is not absolute: %q", path)
	}
}

func TestItEnv_test(t *testing.T) {
	if rspath.IsAbsolute("test-path") {
		t.Errorf("test-path is absolute")
	}

	if _, err := CurrentDir(); err != nil {
		t.Fatalf("current_dir: %v", err)
	}
}

func splitPathsUnder(flavor rspath.Flavor, unparsed string) []string {
	split := EnvSplitPaths{inner: sysSplitPathsNew(flavor, unparsed)}
	return split.Collect()
}

func TestItEnv_split_paths_windows(t *testing.T) {
	checkParse := func(unparsed string, parsed []string) bool {
		return slices.Equal(splitPathsUnder(rspath.Windows, unparsed), parsed)
	}

	for _, c := range []struct {
		unparsed string
		parsed   []string
	}{
		{"", []string{""}},
		{`""`, []string{""}},
		{";;", []string{"", "", ""}},
		{`c:\`, []string{`c:\`}},
		{`c:\;`, []string{`c:\`, ""}},
		{`c:\;c:\Program Files\`, []string{`c:\`, `c:\Program Files\`}},
		{`c:\;c:\"foo"\`, []string{`c:\`, `c:\foo\`}},
		{`c:\;c:\"foo;bar"\;c:\baz`, []string{`c:\`, `c:\foo;bar\`, `c:\baz`}},
	} {
		if !checkParse(c.unparsed, c.parsed) {
			t.Errorf("split_paths(%q) = %q, expected %q", c.unparsed, splitPathsUnder(rspath.Windows, c.unparsed), c.parsed)
		}
	}
}

func TestItEnv_split_paths_unix(t *testing.T) {
	checkParse := func(unparsed string, parsed []string) bool {
		return slices.Equal(splitPathsUnder(rspath.Unix, unparsed), parsed)
	}

	for _, c := range []struct {
		unparsed string
		parsed   []string
	}{
		{"", []string{""}},
		{"::", []string{"", "", ""}},
		{"/", []string{"/"}},
		{"/:", []string{"/", ""}},
		{"/:/usr/local", []string{"/", "/usr/local"}},
	} {
		if !checkParse(c.unparsed, c.parsed) {
			t.Errorf("split_paths(%q) = %q, expected %q", c.unparsed, splitPathsUnder(rspath.Unix, c.unparsed), c.parsed)
		}
	}
}

func TestItEnv_join_paths_unix(t *testing.T) {
	testEq := func(input []string, output string) bool {
		joined, err := sysJoinPaths(rspath.Unix, input)
		return err == nil && joined == output
	}

	if !testEq([]string{}, "") {
		t.Errorf("join_paths([])")
	}
	if !testEq([]string{"/bin", "/usr/bin", "/usr/local/bin"}, "/bin:/usr/bin:/usr/local/bin") {
		t.Errorf("join_paths of three paths")
	}
	if !testEq([]string{"", "/bin", "", "", "/usr/bin", ""}, ":/bin:::/usr/bin:") {
		t.Errorf("join_paths with empty paths")
	}
	if _, err := sysJoinPaths(rspath.Unix, []string{"/te:st"}); err == nil {
		t.Errorf("join_paths of a path holding the separator succeeded")
	}
}

func TestItEnv_join_paths_windows(t *testing.T) {
	testEq := func(input []string, output string) bool {
		joined, err := sysJoinPaths(rspath.Windows, input)
		return err == nil && joined == output
	}

	if !testEq([]string{}, "") {
		t.Errorf("join_paths([])")
	}
	if !testEq([]string{`c:\windows`, `c:\`}, `c:\windows;c:\`) {
		t.Errorf("join_paths of two paths")
	}
	if !testEq([]string{"", `c:\windows`, "", "", `c:\`, ""}, `;c:\windows;;;c:\;`) {
		t.Errorf("join_paths with empty paths")
	}
	if !testEq([]string{`c:\te;st`, `c:\`}, `"c:\te;st";c:\`) {
		t.Errorf("join_paths of a path holding the separator")
	}
	if _, err := sysJoinPaths(rspath.Windows, []string{`c:\te"st`}); err == nil {
		t.Errorf("join_paths of a path holding a quote succeeded")
	}
}

func TestItEnv_args_debug(t *testing.T) {
	left := rs.Format("Args {{ inner: {:?} }}", Args().Collect())
	right := rs.Format("{:?}", Args())
	if left != right {
		t.Errorf("%s != %s", left, right)
	}
}

func TestItEnv_args_os_debug(t *testing.T) {
	left := rs.Format("ArgsOs {{ inner: {:?} }}", ArgsOs().Collect())
	right := rs.Format("{:?}", ArgsOs())
	if left != right {
		t.Errorf("%s != %s", left, right)
	}
}

func TestItEnv_vars_debug(t *testing.T) {
	left := rs.Format("Vars {{ inner: {:?} }}", Vars().Collect())
	right := rs.Format("{:?}", Vars())
	if left != right {
		t.Errorf("%s != %s", left, right)
	}
}

func TestItEnv_vars_os_debug(t *testing.T) {
	left := rs.Format("VarsOs {{ inner: {:?} }}", VarsOs().Collect())
	right := rs.Format("{:?}", VarsOs())
	if left != right {
		t.Errorf("%s != %s", left, right)
	}
}
