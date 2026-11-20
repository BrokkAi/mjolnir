// Port of std 1.99.1 tests/path.rs
//
// Rust compiles one path syntax into std and guards its tests with cfg. This
// package holds both syntaxes, so a test Rust runs on Unix runs here under
// the Unix flavour, a test it runs on Windows under the Windows flavour, and
// a test it runs everywhere under both, on any host.
//
// Not ported, because what they test does not exist in Go:
// test_decompositions_cygwin (no Cygwin flavour here), and the allocation
// assertions of test_pathbuf_leak, test_clone_into and clone_to_uninit. The
// conversions between Path, PathBuf, Box<Path>, Rc<Path>, Arc<Path> and
// Cow<Path> (into, into_boxed, into_rc) are the identity on a Go string;
// their assertions are kept as equality checks.
//
// test_unix_absolute and test_windows_absolute read the current directory
// and are in it_path_unix_test.go and it_path_windows_test.go.

package rspath

import (
	"slices"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

func some(s string) rs.Option[string] { return rs.Some(s) }

var none = rs.None[string]()

var bothFlavors = []Flavor{Unix, Windows}

// formatterReady reports whether rs.Format is the real formatter. While it
// is the placeholder, the assertions that go through it are skipped and the
// same text is checked through this package's own String and DebugString.
func formatterReady() (ready bool) {
	defer func() {
		if recover() != nil {
			ready = false
		}
	}()
	return rs.Format("{}-{:>3}", 1, "a") == "1-  a"
}

// The six arms of the t! macro.

func tIter(t *testing.T, F Flavor, path string, iter []string) {
	t.Helper()

	// Forward iteration
	var comps []string
	for p := range F.Iter(path).All() {
		comps = append(comps, F.ToStringLossy(p))
	}
	exps := iter
	if !slices.Equal(comps, exps) {
		t.Errorf("%v %q iter: Expected %q, found %q", F, path, exps, comps)
	}

	// Reverse iteration
	comps = nil
	for p := range F.Iter(path).Rev() {
		comps = append(comps, F.ToStringLossy(p))
	}
	exps = slices.Clone(exps)
	slices.Reverse(exps)
	if !slices.Equal(comps, exps) {
		t.Errorf("%v %q iter().rev(): Expected %q, found %q", F, path, exps, comps)
	}
}

func tRoot(t *testing.T, F Flavor, path string, hasRoot, isAbsolute bool) {
	t.Helper()

	actRoot := F.HasRoot(path)
	if actRoot != hasRoot {
		t.Errorf("%v %q has_root: Expected %v, found %v", F, path, hasRoot, actRoot)
	}

	actAbs := F.IsAbsolute(path)
	if actAbs != isAbsolute {
		t.Errorf("%v %q is_absolute: Expected %v, found %v", F, path, isAbsolute, actAbs)
	}
}

// toStrUnwrap is `.map(|p| p.to_str().unwrap())` on an Option<&OsStr>.
func toStrUnwrap(F Flavor, value rs.Option[string]) rs.Option[string] {
	return rs.OptionMap(value, func(p string) string { return F.ToStr(p).Unwrap() })
}

func tParent(t *testing.T, F Flavor, path string, expParent, expFile rs.Option[string]) {
	t.Helper()

	parent := toStrUnwrap(F, F.Parent(path))
	if parent != expParent {
		t.Errorf("%v %q parent: Expected %v, found %v", F, path, debugOption(expParent), debugOption(parent))
	}

	file := toStrUnwrap(F, F.FileName(path))
	if file != expFile {
		t.Errorf("%v %q file_name: Expected %v, found %v", F, path, debugOption(expFile), debugOption(file))
	}
}

func tStem(t *testing.T, F Flavor, path string, expStem, expExt rs.Option[string]) {
	t.Helper()

	stem := toStrUnwrap(F, F.FileStem(path))
	if stem != expStem {
		t.Errorf("%v %q file_stem: Expected %v, found %v", F, path, debugOption(expStem), debugOption(stem))
	}

	ext := toStrUnwrap(F, F.Extension(path))
	if ext != expExt {
		t.Errorf("%v %q extension: Expected %v, found %v", F, path, debugOption(expExt), debugOption(ext))
	}
}

func tPrefix(t *testing.T, F Flavor, path string, expPrefix, expExt rs.Option[string]) {
	t.Helper()

	prefix := toStrUnwrap(F, F.FilePrefix(path))
	if prefix != expPrefix {
		t.Errorf("%v %q file_prefix: Expected %v, found %v", F, path, debugOption(expPrefix), debugOption(prefix))
	}

	ext := toStrUnwrap(F, F.Extension(path))
	if ext != expExt {
		t.Errorf("%v %q extension: Expected %v, found %v", F, path, debugOption(expExt), debugOption(ext))
	}
}

func tAll(t *testing.T, F Flavor, path string, iter []string, hasRoot, isAbsolute bool,
	parent, file, fileStem, extension, filePrefix rs.Option[string]) {
	t.Helper()
	tIter(t, F, path, iter)
	tRoot(t, F, path, hasRoot, isAbsolute)
	tParent(t, F, path, parent, file)
	tStem(t, F, path, fileStem, extension)
	tPrefix(t, F, path, filePrefix, extension)
}

func debugOption(value rs.Option[string]) string {
	if s, ok := value.Get(); ok {
		return "Some(" + Unix.DebugString(s) + ")"
	}
	return "None"
}

func TestItPath_into(t *testing.T) {
	for _, F := range bothFlavors {
		staticPath := "/home/foo"
		staticCowPath := staticPath
		pathbuf := "/home/foo"

		{
			path := pathbuf
			borrowedCowPath := path

			if !F.Eq(staticCowPath, borrowedCowPath) {
				t.Errorf("%v: %q != %q", F, staticCowPath, borrowedCowPath)
			}
		}

		ownedCowPath := pathbuf

		if !F.Eq(staticCowPath, ownedCowPath) {
			t.Errorf("%v: %q != %q", F, staticCowPath, ownedCowPath)
		}
	}
}

func TestItPath_test_pathbuf_leak(t *testing.T) {
	// A Go string is not owned or leaked. What is left of the test is that
	// the bytes of the path are the bytes of the string it was made from.
	str := "/have/a/cake"
	buf := str
	leaked := buf
	if got := string(OsStrAsEncodedBytes(leaked)); got != "/have/a/cake" {
		t.Errorf("as_encoded_bytes: %q", got)
	}
}

func TestItPath_test_decompositions_unix(t *testing.T) {
	F := Unix
	tAll(t, F, "", nil, false, false, none, none, none, none, none)

	tAll(t, F, "foo", []string{"foo"}, false, false, some(""), some("foo"), some("foo"), none, some("foo"))

	tAll(t, F, "/", []string{"/"}, true, true, none, none, none, none, none)

	tAll(t, F, "/foo", []string{"/", "foo"}, true, true, some("/"), some("foo"), some("foo"), none, some("foo"))

	tAll(t, F, "foo/", []string{"foo"}, false, false, some(""), some("foo"), some("foo"), none, some("foo"))

	tAll(t, F, "/foo/", []string{"/", "foo"}, true, true, some("/"), some("foo"), some("foo"), none, some("foo"))

	tAll(t, F, "foo/bar", []string{"foo", "bar"}, false, false, some("foo"), some("bar"), some("bar"), none, some("bar"))

	tAll(t, F, "/foo/bar", []string{"/", "foo", "bar"}, true, true, some("/foo"), some("bar"), some("bar"), none, some("bar"))

	tAll(t, F, "///foo///", []string{"/", "foo"}, true, true, some("/"), some("foo"), some("foo"), none, some("foo"))

	tAll(t, F, "///foo///bar", []string{"/", "foo", "bar"}, true, true, some("///foo"), some("bar"), some("bar"), none, some("bar"))

	tAll(t, F, "./.", []string{"."}, false, false, some(""), none, none, none, none)

	tAll(t, F, "/..", []string{"/", ".."}, true, true, some("/"), none, none, none, none)

	tAll(t, F, "../", []string{".."}, false, false, some(""), none, none, none, none)

	tAll(t, F, "foo/.", []string{"foo"}, false, false, some(""), some("foo"), some("foo"), none, some("foo"))

	tAll(t, F, "foo/..", []string{"foo", ".."}, false, false, some("foo"), none, none, none, none)

	tAll(t, F, "foo/./", []string{"foo"}, false, false, some(""), some("foo"), some("foo"), none, some("foo"))

	tAll(t, F, "foo/./bar", []string{"foo", "bar"}, false, false, some("foo"), some("bar"), some("bar"), none, some("bar"))

	tAll(t, F, "foo/../", []string{"foo", ".."}, false, false, some("foo"), none, none, none, none)

	tAll(t, F, "foo/../bar", []string{"foo", "..", "bar"}, false, false, some("foo/.."), some("bar"), some("bar"), none, some("bar"))

	tAll(t, F, "./a", []string{".", "a"}, false, false, some("."), some("a"), some("a"), none, some("a"))

	tAll(t, F, ".", []string{"."}, false, false, some(""), none, none, none, none)

	tAll(t, F, "./", []string{"."}, false, false, some(""), none, none, none, none)

	tAll(t, F, "a/b", []string{"a", "b"}, false, false, some("a"), some("b"), some("b"), none, some("b"))

	tAll(t, F, "a//b", []string{"a", "b"}, false, false, some("a"), some("b"), some("b"), none, some("b"))

	tAll(t, F, "a/./b", []string{"a", "b"}, false, false, some("a"), some("b"), some("b"), none, some("b"))

	tAll(t, F, "a/b/c", []string{"a", "b", "c"}, false, false, some("a/b"), some("c"), some("c"), none, some("c"))

	tAll(t, F, ".foo", []string{".foo"}, false, false, some(""), some(".foo"), some(".foo"), none, some(".foo"))

	tAll(t, F, "a/.foo", []string{"a", ".foo"}, false, false, some("a"), some(".foo"), some(".foo"), none, some(".foo"))

	tAll(t, F, "a/.rustfmt.toml", []string{"a", ".rustfmt.toml"}, false, false, some("a"), some(".rustfmt.toml"), some(".rustfmt"), some("toml"), some(".rustfmt"))

	tAll(t, F, "a/.x.y.z", []string{"a", ".x.y.z"}, false, false, some("a"), some(".x.y.z"), some(".x.y"), some("z"), some(".x"))
}

func TestItPath_test_decompositions_windows(t *testing.T) {
	F := Windows
	tAll(t, F, "", nil, false, false, none, none, none, none, none)

	tAll(t, F, "foo", []string{"foo"}, false, false, some(""), some("foo"), some("foo"), none, some("foo"))

	tAll(t, F, "/", []string{"\\"}, true, false, none, none, none, none, none)

	tAll(t, F, "\\", []string{"\\"}, true, false, none, none, none, none, none)

	tAll(t, F, "c:", []string{"c:"}, false, false, none, none, none, none, none)

	tAll(t, F, "c:\\", []string{"c:", "\\"}, true, true, none, none, none, none, none)

	tAll(t, F, "c:/", []string{"c:", "\\"}, true, true, none, none, none, none, none)

	tAll(t, F, "/foo", []string{"\\", "foo"}, true, false, some("/"), some("foo"), some("foo"), none, some("foo"))

	tAll(t, F, "foo/", []string{"foo"}, false, false, some(""), some("foo"), some("foo"), none, some("foo"))

	tAll(t, F, "/foo/", []string{"\\", "foo"}, true, false, some("/"), some("foo"), some("foo"), none, some("foo"))

	tAll(t, F, "foo/bar", []string{"foo", "bar"}, false, false, some("foo"), some("bar"), some("bar"), none, some("bar"))

	tAll(t, F, "/foo/bar", []string{"\\", "foo", "bar"}, true, false, some("/foo"), some("bar"), some("bar"), none, some("bar"))

	tAll(t, F, "///foo///", []string{"\\", "foo"}, true, false, some("/"), some("foo"), some("foo"), none, some("foo"))

	tAll(t, F, "///foo///bar", []string{"\\", "foo", "bar"}, true, false, some("///foo"), some("bar"), some("bar"), none, some("bar"))

	tAll(t, F, "./.", []string{"."}, false, false, some(""), none, none, none, none)

	tAll(t, F, "/..", []string{"\\", ".."}, true, false, some("/"), none, none, none, none)

	tAll(t, F, "../", []string{".."}, false, false, some(""), none, none, none, none)

	tAll(t, F, "foo/.", []string{"foo"}, false, false, some(""), some("foo"), some("foo"), none, some("foo"))

	tAll(t, F, "foo/..", []string{"foo", ".."}, false, false, some("foo"), none, none, none, none)

	tAll(t, F, "foo/./", []string{"foo"}, false, false, some(""), some("foo"), some("foo"), none, some("foo"))

	tAll(t, F, "foo/./bar", []string{"foo", "bar"}, false, false, some("foo"), some("bar"), some("bar"), none, some("bar"))

	tAll(t, F, "foo/../", []string{"foo", ".."}, false, false, some("foo"), none, none, none, none)

	tAll(t, F, "foo/../bar", []string{"foo", "..", "bar"}, false, false, some("foo/.."), some("bar"), some("bar"), none, some("bar"))

	tAll(t, F, "./a", []string{".", "a"}, false, false, some("."), some("a"), some("a"), none, some("a"))

	tAll(t, F, ".", []string{"."}, false, false, some(""), none, none, none, none)

	tAll(t, F, "./", []string{"."}, false, false, some(""), none, none, none, none)

	tAll(t, F, "a/b", []string{"a", "b"}, false, false, some("a"), some("b"), some("b"), none, some("b"))

	tAll(t, F, "a//b", []string{"a", "b"}, false, false, some("a"), some("b"), some("b"), none, some("b"))

	tAll(t, F, "a/./b", []string{"a", "b"}, false, false, some("a"), some("b"), some("b"), none, some("b"))

	tAll(t, F, "a/b/c", []string{"a", "b", "c"}, false, false, some("a/b"), some("c"), some("c"), none, some("c"))

	tAll(t, F, "a\\b\\c", []string{"a", "b", "c"}, false, false, some("a\\b"), some("c"), some("c"), none, some("c"))

	tAll(t, F, "\\a", []string{"\\", "a"}, true, false, some("\\"), some("a"), some("a"), none, some("a"))

	tAll(t, F, "c:\\foo.txt", []string{"c:", "\\", "foo.txt"}, true, true, some("c:\\"), some("foo.txt"), some("foo"), some("txt"), some("foo"))

	tAll(t, F, "\\\\server\\share\\foo.txt", []string{"\\\\server\\share", "\\", "foo.txt"}, true, true, some("\\\\server\\share\\"), some("foo.txt"), some("foo"), some("txt"), some("foo"))

	tAll(t, F, "\\\\server\\share", []string{"\\\\server\\share", "\\"}, true, true, none, none, none, none, none)

	tAll(t, F, "\\\\server", []string{"\\", "server"}, true, false, some("\\"), some("server"), some("server"), none, some("server"))

	tAll(t, F, "\\\\?\\bar\\foo.txt", []string{"\\\\?\\bar", "\\", "foo.txt"}, true, true, some("\\\\?\\bar\\"), some("foo.txt"), some("foo"), some("txt"), some("foo"))

	tAll(t, F, "\\\\?\\bar", []string{"\\\\?\\bar"}, true, true, none, none, none, none, none)

	tAll(t, F, "\\\\?\\", []string{"\\\\?\\"}, true, true, none, none, none, none, none)

	tAll(t, F, "\\\\?\\UNC\\server\\share\\foo.txt", []string{"\\\\?\\UNC\\server\\share", "\\", "foo.txt"}, true, true, some("\\\\?\\UNC\\server\\share\\"), some("foo.txt"), some("foo"), some("txt"), some("foo"))

	tAll(t, F, "\\\\?\\UNC\\server", []string{"\\\\?\\UNC\\server"}, true, true, none, none, none, none, none)

	tAll(t, F, "\\\\?\\UNC\\", []string{"\\\\?\\UNC\\"}, true, true, none, none, none, none, none)

	tAll(t, F, "\\\\?\\C:\\foo.txt", []string{"\\\\?\\C:", "\\", "foo.txt"}, true, true, some("\\\\?\\C:\\"), some("foo.txt"), some("foo"), some("txt"), some("foo"))

	tAll(t, F, "\\\\?\\C:\\", []string{"\\\\?\\C:", "\\"}, true, true, none, none, none, none, none)

	tAll(t, F, "\\\\?\\C:", []string{"\\\\?\\C:"}, true, true, none, none, none, none, none)

	tAll(t, F, "\\\\?\\foo/bar", []string{"\\\\?\\foo/bar"}, true, true, none, none, none, none, none)

	tAll(t, F, "\\\\?\\C:/foo/bar", []string{"\\\\?\\C:", "\\", "foo/bar"}, true, true, some("\\\\?\\C:/"), some("foo/bar"), some("foo/bar"), none, some("foo/bar"))

	tAll(t, F, "\\\\.\\foo\\bar", []string{"\\\\.\\foo", "\\", "bar"}, true, true, some("\\\\.\\foo\\"), some("bar"), some("bar"), none, some("bar"))

	tAll(t, F, "\\\\.\\foo", []string{"\\\\.\\foo", "\\"}, true, true, none, none, none, none, none)

	tAll(t, F, "\\\\.\\foo/bar", []string{"\\\\.\\foo", "\\", "bar"}, true, true, some("\\\\.\\foo/"), some("bar"), some("bar"), none, some("bar"))

	tAll(t, F, "\\\\.\\foo\\bar/baz", []string{"\\\\.\\foo", "\\", "bar", "baz"}, true, true, some("\\\\.\\foo\\bar"), some("baz"), some("baz"), none, some("baz"))

	tAll(t, F, "\\\\.\\", []string{"\\\\.\\", "\\"}, true, true, none, none, none, none, none)

	tAll(t, F, "\\\\?\\a\\b\\", []string{"\\\\?\\a", "\\", "b"}, true, true, some("\\\\?\\a\\"), some("b"), some("b"), none, some("b"))

	tAll(t, F, "\\\\?\\C:\\foo.txt.zip", []string{"\\\\?\\C:", "\\", "foo.txt.zip"}, true, true, some("\\\\?\\C:\\"), some("foo.txt.zip"), some("foo.txt"), some("zip"), some("foo"))

	tAll(t, F, "\\\\?\\C:\\.foo.txt.zip", []string{"\\\\?\\C:", "\\", ".foo.txt.zip"}, true, true, some("\\\\?\\C:\\"), some(".foo.txt.zip"), some(".foo.txt"), some("zip"), some(".foo"))

	tAll(t, F, "\\\\?\\C:\\.foo", []string{"\\\\?\\C:", "\\", ".foo"}, true, true, some("\\\\?\\C:\\"), some(".foo"), some(".foo"), none, some(".foo"))

	tAll(t, F, "a/.x.y.z", []string{"a", ".x.y.z"}, false, false, some("a"), some(".x.y.z"), some(".x.y"), some("z"), some(".x"))
}

// Unix paths are tested in `test_decompositions_unix` above.

func TestItPath_test_stem_ext(t *testing.T) {
	for _, F := range bothFlavors {
		tStem(t, F, "foo", some("foo"), none)

		tStem(t, F, "foo.", some("foo"), some(""))

		tStem(t, F, ".foo", some(".foo"), none)

		tStem(t, F, "foo.txt", some("foo"), some("txt"))

		tStem(t, F, "foo.bar.txt", some("foo.bar"), some("txt"))

		tStem(t, F, "foo.bar.", some("foo.bar"), some(""))

		tStem(t, F, ".", none, none)

		tStem(t, F, "..", none, none)

		tStem(t, F, ".x.y.z", some(".x.y"), some("z"))

		tStem(t, F, "..x.y.z", some("..x.y"), some("z"))

		tStem(t, F, "", none, none)
	}
}

func TestItPath_test_prefix_ext(t *testing.T) {
	for _, F := range bothFlavors {
		tPrefix(t, F, "foo", some("foo"), none)

		tPrefix(t, F, "foo.", some("foo"), some(""))

		tPrefix(t, F, ".foo", some(".foo"), none)

		tPrefix(t, F, "foo.txt", some("foo"), some("txt"))

		tPrefix(t, F, "foo.bar.txt", some("foo"), some("txt"))

		tPrefix(t, F, "foo.bar.", some("foo"), some(""))

		tPrefix(t, F, ".", none, none)

		tPrefix(t, F, "..", none, none)

		tPrefix(t, F, ".x.y.z", some(".x"), some("z"))

		tPrefix(t, F, "..x.y.z", some("."), some("z"))

		tPrefix(t, F, "", none, none)
	}
}

func TestItPath_test_push(t *testing.T) {
	tp := func(F Flavor, path, push, expected string) {
		t.Helper()
		actual := path
		F.Push(&actual, push)
		if F.ToStr(actual) != some(expected) {
			t.Errorf("%v: pushing %q onto %q: Expected %q, got %q", F, push, path, expected, actual)
		}
	}

	// if cfg!(unix)
	{
		F := Unix
		tp(F, "", "foo", "foo")
		tp(F, "foo", "bar", "foo/bar")
		tp(F, "foo/", "bar", "foo/bar")
		tp(F, "foo//", "bar", "foo//bar")
		tp(F, "foo/.", "bar", "foo/./bar")
		tp(F, "foo./.", "bar", "foo././bar")
		tp(F, "foo", "", "foo/")
		tp(F, "foo", ".", "foo/.")
		tp(F, "foo", "..", "foo/..")
		tp(F, "foo", "/", "/")
		tp(F, "/foo/bar", "/", "/")
		tp(F, "/foo/bar", "/baz", "/baz")
		tp(F, "/foo/bar", "./baz", "/foo/bar/./baz")

		// The two cases under `if cfg!(target_os = "cygwin")` are not ported:
		// there is no Cygwin flavour here.
	}
	// else
	{
		F := Windows
		tp(F, "", "foo", "foo")
		tp(F, "foo", "bar", "foo\\bar")
		tp(F, "foo/", "bar", "foo/bar")
		tp(F, "foo\\", "bar", "foo\\bar")
		tp(F, "foo//", "bar", "foo//bar")
		tp(F, "foo\\\\", "bar", "foo\\\\bar")
		tp(F, "foo/.", "bar", "foo/.\\bar")
		tp(F, "foo./.", "bar", "foo./.\\bar")
		tp(F, "foo\\.", "bar", "foo\\.\\bar")
		tp(F, "foo.\\.", "bar", "foo.\\.\\bar")
		tp(F, "foo", "", "foo\\")
		tp(F, "foo", ".", "foo\\.")
		tp(F, "foo", "..", "foo\\..")
		tp(F, "foo", "/", "/")
		tp(F, "foo", "\\", "\\")
		tp(F, "/foo/bar", "/", "/")
		tp(F, "\\foo\\bar", "\\", "\\")
		tp(F, "/foo/bar", "/baz", "/baz")
		tp(F, "/foo/bar", "\\baz", "\\baz")
		tp(F, "/foo/bar", "./baz", "/foo/bar\\./baz")
		tp(F, "/foo/bar", ".\\baz", "/foo/bar\\.\\baz")

		tp(F, "c:\\", "windows", "c:\\windows")
		tp(F, "c:", "windows", "c:windows")

		tp(F, "a\\b\\c", "d", "a\\b\\c\\d")
		tp(F, "\\a\\b\\c", "d", "\\a\\b\\c\\d")
		tp(F, "a\\b", "c\\d", "a\\b\\c\\d")
		tp(F, "a\\b", "\\c\\d", "\\c\\d")
		tp(F, "a\\b", ".", "a\\b\\.")
		tp(F, "a\\b", "..\\c", "a\\b\\..\\c")
		tp(F, "a\\b", "C:a.txt", "C:a.txt")
		tp(F, "a\\b", "C:\\a.txt", "C:\\a.txt")
		tp(F, "C:\\a", "C:\\b.txt", "C:\\b.txt")
		tp(F, "C:\\a\\b\\c", "C:d", "C:d")
		tp(F, "C:a\\b\\c", "C:d", "C:d")
		tp(F, "C:", "a\\b\\c", "C:a\\b\\c")
		tp(F, "C:", "..\\a", "C:..\\a")
		tp(F, "\\\\server\\share\\foo", "bar", "\\\\server\\share\\foo\\bar")
		tp(F, "\\\\server\\share\\foo", "C:baz", "C:baz")
		tp(F, "\\\\?\\C:\\a\\b", "C:c\\d", "C:c\\d")
		tp(F, "\\\\?\\C:a\\b", "C:c\\d", "C:c\\d")
		tp(F, "\\\\?\\C:\\a\\b", "C:\\c\\d", "C:\\c\\d")
		tp(F, "\\\\?\\foo\\bar", "baz", "\\\\?\\foo\\bar\\baz")
		tp(F, "\\\\?\\UNC\\server\\share\\foo", "bar", "\\\\?\\UNC\\server\\share\\foo\\bar")
		tp(F, "\\\\?\\UNC\\server\\share", "C:\\a", "C:\\a")
		tp(F, "\\\\?\\UNC\\server\\share", "C:a", "C:a")

		// Note: modified from old path API
		tp(F, "\\\\?\\UNC\\server", "foo", "\\\\?\\UNC\\server\\foo")

		tp(F, "C:\\a", "\\\\?\\UNC\\server\\share", "\\\\?\\UNC\\server\\share")
		tp(F, "\\\\.\\foo\\bar", "baz", "\\\\.\\foo\\bar\\baz")
		tp(F, "\\\\.\\foo\\bar", "C:a", "C:a")
		// again, not sure about the following, but I'm assuming \\.\ should be verbatim
		tp(F, "\\\\.\\foo", "..\\bar", "\\\\.\\foo\\..\\bar")

		tp(F, "\\\\?\\C:", "foo", "\\\\?\\C:\\foo")

		tp(F, "\\\\?\\C:\\bar", "../foo", "\\\\?\\C:\\foo")
		tp(F, "\\\\?\\C:\\bar", "../../foo", "\\\\?\\C:\\foo")
		tp(F, "\\\\?\\C:\\", "../foo", "\\\\?\\C:\\foo")
		tp(F, "\\\\?\\C:", "D:\\foo/./", "D:\\foo/./")
		tp(F, "\\\\?\\C:", "\\\\?\\D:\\foo\\.\\", "\\\\?\\D:\\foo\\.\\")
		tp(F, "\\\\?\\A:\\x\\y", "/foo", "\\\\?\\A:\\foo")
		tp(F, "\\\\?\\A:", "..\\foo\\.", "\\\\?\\A:\\foo")
		tp(F, "\\\\?\\A:\\x\\y", ".\\foo\\.", "\\\\?\\A:\\x\\y\\foo")
		tp(F, "\\\\?\\A:\\x\\y", "", "\\\\?\\A:\\x\\y\\")
	}
}

func TestItPath_test_pop(t *testing.T) {
	tp := func(F Flavor, path, expected string, expectedOutput bool) {
		t.Helper()
		actual := path
		output := F.Pop(&actual)
		if !(F.ToStr(actual) == some(expected) && output == expectedOutput) {
			t.Errorf("%v: popping from %q: Expected %q/%v, got %q/%v", F, path, expected, expectedOutput, actual, output)
		}
	}

	for _, F := range bothFlavors {
		tp(F, "", "", false)
		tp(F, "/", "/", false)
		tp(F, "foo", "", true)
		tp(F, ".", "", true)
		tp(F, "/foo", "/", true)
		tp(F, "/foo/bar", "/foo", true)
		tp(F, "foo/bar", "foo", true)
		tp(F, "foo/.", "", true)
		tp(F, "foo//bar", "foo", true)
	}

	// if cfg!(windows)
	{
		F := Windows
		tp(F, "a\\b\\c", "a\\b", true)
		tp(F, "\\a", "\\", true)
		tp(F, "\\", "\\", false)

		tp(F, "C:\\a\\b", "C:\\a", true)
		tp(F, "C:\\a", "C:\\", true)
		tp(F, "C:\\", "C:\\", false)
		tp(F, "C:a\\b", "C:a", true)
		tp(F, "C:a", "C:", true)
		tp(F, "C:", "C:", false)
		tp(F, "\\\\server\\share\\a\\b", "\\\\server\\share\\a", true)
		tp(F, "\\\\server\\share\\a", "\\\\server\\share\\", true)
		tp(F, "\\\\server\\share", "\\\\server\\share", false)
		tp(F, "\\\\?\\a\\b\\c", "\\\\?\\a\\b", true)
		tp(F, "\\\\?\\a\\b", "\\\\?\\a\\", true)
		tp(F, "\\\\?\\a", "\\\\?\\a", false)
		tp(F, "\\\\?\\C:\\a\\b", "\\\\?\\C:\\a", true)
		tp(F, "\\\\?\\C:\\a", "\\\\?\\C:\\", true)
		tp(F, "\\\\?\\C:\\", "\\\\?\\C:\\", false)
		tp(F, "\\\\?\\UNC\\server\\share\\a\\b", "\\\\?\\UNC\\server\\share\\a", true)
		tp(F, "\\\\?\\UNC\\server\\share\\a", "\\\\?\\UNC\\server\\share\\", true)
		tp(F, "\\\\?\\UNC\\server\\share", "\\\\?\\UNC\\server\\share", false)
		tp(F, "\\\\.\\a\\b\\c", "\\\\.\\a\\b", true)
		tp(F, "\\\\.\\a\\b", "\\\\.\\a\\", true)
		tp(F, "\\\\.\\a", "\\\\.\\a", false)

		tp(F, "\\\\?\\a\\b\\", "\\\\?\\a\\", true)
	}
}

func TestItPath_test_set_file_name(t *testing.T) {
	tfn := func(F Flavor, path, file, expected string) {
		t.Helper()
		p := path
		F.SetFileName(&p, file)
		if F.ToStr(p) != some(expected) {
			t.Errorf("%v: setting file name of %q to %q: Expected %q, got %q", F, path, file, expected, p)
		}
	}

	for _, F := range bothFlavors {
		tfn(F, "foo", "foo", "foo")
		tfn(F, "foo", "bar", "bar")
		tfn(F, "foo", "", "")
		tfn(F, "", "foo", "foo")
	}
	// if cfg!(unix)
	{
		F := Unix
		tfn(F, ".", "foo", "./foo")
		tfn(F, "foo/", "bar", "bar")
		tfn(F, "foo/.", "bar", "bar")
		tfn(F, "..", "foo", "../foo")
		tfn(F, "foo/..", "bar", "foo/../bar")
		tfn(F, "/", "foo", "/foo")
	}
	// else
	{
		F := Windows
		tfn(F, ".", "foo", ".\\foo")
		tfn(F, "foo\\", "bar", "bar")
		tfn(F, "foo\\.", "bar", "bar")
		tfn(F, "..", "foo", "..\\foo")
		tfn(F, "foo\\..", "bar", "foo\\..\\bar")
		tfn(F, "\\", "foo", "\\foo")
	}
}

func TestItPath_test_set_extension(t *testing.T) {
	tfe := func(F Flavor, path, ext, expected string, expectedOutput bool) {
		t.Helper()
		p := path
		output := F.SetExtension(&p, ext)
		if !(F.ToStr(p) == some(expected) && output == expectedOutput) {
			t.Errorf("%v: setting extension of %q to %q: Expected %q/%v, got %q/%v",
				F, path, ext, expected, expectedOutput, p, output)
		}
	}

	for _, F := range bothFlavors {
		tfe(F, "foo", "txt", "foo.txt", true)
		tfe(F, "foo.bar", "txt", "foo.txt", true)
		tfe(F, "foo.bar.baz", "txt", "foo.bar.txt", true)
		tfe(F, ".test", "txt", ".test.txt", true)
		tfe(F, "foo.txt", "", "foo", true)
		tfe(F, "foo", "", "foo", true)
		tfe(F, "", "foo", "", false)
		tfe(F, ".", "foo", ".", false)
		tfe(F, "foo/", "bar", "foo.bar", true)
		tfe(F, "foo/.", "bar", "foo.bar", true)
		tfe(F, "..", "foo", "..", false)
		tfe(F, "foo/..", "bar", "foo/..", false)
		tfe(F, "/", "foo", "/", false)
	}
}

func TestItPath_test_add_extension(t *testing.T) {
	tfe := func(F Flavor, path, ext, expected string, expectedOutput bool) {
		t.Helper()
		p := path
		output := F.AddExtension(&p, ext)
		if !(F.ToStr(p) == some(expected) && output == expectedOutput) {
			t.Errorf("%v: adding extension of %q to %q: Expected %q/%v, got %q/%v",
				F, path, ext, expected, expectedOutput, p, output)
		}
	}

	for _, F := range bothFlavors {
		tfe(F, "foo", "txt", "foo.txt", true)
		tfe(F, "foo.bar", "txt", "foo.bar.txt", true)
		tfe(F, "foo.bar.baz", "txt", "foo.bar.baz.txt", true)
		tfe(F, ".test", "txt", ".test.txt", true)
		tfe(F, "foo.txt", "", "foo.txt", true)
		tfe(F, "foo", "", "foo", true)
		tfe(F, "", "foo", "", false)
		tfe(F, ".", "foo", ".", false)
		tfe(F, "foo/", "bar", "foo.bar", true)
		tfe(F, "foo/.", "bar", "foo.bar", true)
		tfe(F, "..", "foo", "..", false)
		tfe(F, "foo/..", "bar", "foo/..", false)
		tfe(F, "/", "foo", "/", false)

		// edge cases
		tfe(F, "/foo.ext////", "bar", "/foo.ext.bar", true)
	}
}

func TestItPath_test_with_extension(t *testing.T) {
	twe := func(F Flavor, input, extension, expected string) {
		t.Helper()
		output := F.WithExtension(input, extension)

		if F.ToStr(output) != some(expected) {
			t.Errorf("%v: calling Path::new(%q).with_extension(%q): Expected %q, got %q",
				F, input, extension, expected, output)
		}
	}

	for _, F := range bothFlavors {
		twe(F, "foo", "txt", "foo.txt")
		twe(F, "foo.bar", "txt", "foo.txt")
		twe(F, "foo.bar.baz", "txt", "foo.bar.txt")
		twe(F, ".test", "txt", ".test.txt")
		twe(F, "foo.txt", "", "foo")
		twe(F, "foo", "", "foo")
		twe(F, "", "foo", "")
		twe(F, ".", "foo", ".")
		twe(F, "foo/", "bar", "foo.bar")
		twe(F, "foo/.", "bar", "foo.bar")
		twe(F, "..", "foo", "..")
		twe(F, "foo/..", "bar", "foo/..")
		twe(F, "/", "foo", "/")

		// New extension is smaller than file name
		twe(F, "aaa_aaa_aaa", "bbb_bbb", "aaa_aaa_aaa.bbb_bbb")
		// New extension is greater than file name
		twe(F, "bbb_bbb", "aaa_aaa_aaa", "bbb_bbb.aaa_aaa_aaa")

		// New extension is smaller than previous extension
		twe(F, "ccc.aaa_aaa_aaa", "bbb_bbb", "ccc.bbb_bbb")
		// New extension is greater than previous extension
		twe(F, "ccc.bbb_bbb", "aaa_aaa_aaa", "ccc.aaa_aaa_aaa")
	}
}

func TestItPath_test_with_added_extension(t *testing.T) {
	twe := func(F Flavor, input, extension, expected string) {
		t.Helper()
		output := F.WithAddedExtension(input, extension)

		if F.ToStr(output) != some(expected) {
			t.Errorf("%v: calling Path::new(%q).with_added_extension(%q): Expected %q, got %q",
				F, input, extension, expected, output)
		}
	}

	for _, F := range bothFlavors {
		twe(F, "foo", "txt", "foo.txt")
		twe(F, "foo.bar", "txt", "foo.bar.txt")
		twe(F, "foo.bar.baz", "txt", "foo.bar.baz.txt")
		twe(F, ".test", "txt", ".test.txt")
		twe(F, "foo.txt", "", "foo.txt")
		twe(F, "foo", "", "foo")
		twe(F, "", "foo", "")
		twe(F, ".", "foo", ".")
		twe(F, "foo/", "bar", "foo.bar")
		twe(F, "foo/.", "bar", "foo.bar")
		twe(F, "..", "foo", "..")
		twe(F, "foo/..", "bar", "foo/..")
		twe(F, "/", "foo", "/")

		// edge cases
		twe(F, "/foo.ext////", "bar", "/foo.ext.bar")

		// New extension is smaller than file name
		twe(F, "aaa_aaa_aaa", "bbb_bbb", "aaa_aaa_aaa.bbb_bbb")
		// New extension is greater than file name
		twe(F, "bbb_bbb", "aaa_aaa_aaa", "bbb_bbb.aaa_aaa_aaa")

		// New extension is smaller than previous extension
		twe(F, "ccc.aaa_aaa_aaa", "bbb_bbb", "ccc.aaa_aaa_aaa.bbb_bbb")
		// New extension is greater than previous extension
		twe(F, "ccc.bbb_bbb", "aaa_aaa_aaa", "ccc.bbb_bbb.aaa_aaa_aaa")
	}
}

func TestItPath_test_eq_receivers(t *testing.T) {
	for _, F := range bothFlavors {
		borrowed := "foo/bar"
		owned := ""
		F.Push(&owned, "foo")
		F.Push(&owned, "bar")
		borrowedCow := borrowed
		ownedCow := owned

		// The receivers differ in Rust (&Path, PathBuf, Cow<Path>); here
		// every one is a string, and each pair must still compare equal.
		for _, current := range []string{borrowed, owned, borrowedCow, ownedCow} {
			for _, other := range []string{borrowed, owned, borrowedCow, ownedCow} {
				if !F.Eq(current, other) {
					t.Errorf("%v: %q != %q", F, current, other)
				}
			}
		}
	}
}

func TestItPath_test_compare(t *testing.T) {
	// Rust hashes each path with DefaultHasher. HashKey is the port's
	// stand-in for the hash: equal exactly when the paths are.
	hash := func(F Flavor, path string) string {
		return F.HashKey(path)
	}

	tc := func(F Flavor, path1, path2 string, expectedEq, expectedStartsWith, expectedEndsWith bool, exp rs.Option[string]) {
		t.Helper()

		eq := F.Eq(path1, path2)
		if eq != expectedEq {
			t.Errorf("%v: %q == %q, expected %v, got %v", F, path1, path2, expectedEq, eq)
		}
		if expectedEq != (hash(F, path1) == hash(F, path2)) {
			t.Errorf("%v: %q == %q, expected %v, got %q and %q", F, path1, path2, expectedEq, hash(F, path1), hash(F, path2))
		}

		startsWith := F.StartsWith(path1, path2)
		if startsWith != expectedStartsWith {
			t.Errorf("%v: %q.starts_with(%q), expected %v, got %v", F, path1, path2, expectedStartsWith, startsWith)
		}

		endsWith := F.EndsWith(path1, path2)
		if endsWith != expectedEndsWith {
			t.Errorf("%v: %q.ends_with(%q), expected %v, got %v", F, path1, path2, expectedEndsWith, endsWith)
		}

		relativeFrom := toStrUnwrap(F, rs.OkOption(F.StripPrefix(path1, path2)))
		if relativeFrom != exp {
			t.Errorf("%v: %q.strip_prefix(%q), expected %v, got %v", F, path1, path2, debugOption(exp), debugOption(relativeFrom))
		}
	}

	for _, F := range bothFlavors {
		tc(F, "", "", true, true, true, some(""))

		tc(F, "foo", "", false, true, true, some("foo"))

		tc(F, "", "foo", false, false, false, none)

		tc(F, "foo", "foo", true, true, true, some(""))

		tc(F, "foo/", "foo", true, true, true, some(""))

		tc(F, "foo//", "foo", true, true, true, some(""))

		tc(F, "foo///", "foo", true, true, true, some(""))

		tc(F, "foo/.", "foo", true, true, true, some(""))

		tc(F, "foo/./bar", "foo/bar", true, true, true, some(""))

		tc(F, "foo/.//bar", "foo/bar", true, true, true, some(""))

		tc(F, "foo//./bar", "foo/bar", true, true, true, some(""))

		tc(F, "foo/bar", "foo", false, true, false, some("bar"))

		tc(F, "foo/bar", "foobar", false, false, false, none)

		tc(F, "foo/bar/baz", "foo/bar", false, true, false, some("baz"))

		tc(F, "foo/bar", "foo/bar/baz", false, false, false, none)

		tc(F, "./foo/bar/", ".", false, true, false, some("foo/bar"))
	}

	// if cfg!(windows)
	{
		F := Windows
		tc(F, "C:\\src\\rust\\cargo-test\\test\\Cargo.toml", "c:\\src\\rust\\cargo-test\\test", false, true, false, some("Cargo.toml"))

		tc(F, "c:\\foo", "C:\\foo", true, true, true, some(""))

		tc(F, "C:\\foo\\.\\bar.txt", "C:\\foo\\bar.txt", true, true, true, some(""))

		tc(F, "C:\\foo\\.", "C:\\foo", true, true, true, some(""))

		tc(F, "\\\\?\\C:\\foo\\.\\bar.txt", "\\\\?\\C:\\foo\\bar.txt", false, false, false, none)
	}
}

func TestItPath_test_components_debug(t *testing.T) {
	// The test has no cfg, and a leading `/` is a root on both platforms.
	for _, F := range bothFlavors {
		path := "/tmp"

		components := F.Components(path)

		expected := "Components([RootDir, Normal(\"tmp\")])"
		if actual := components.DebugString(); expected != actual {
			t.Errorf("%v: expected %s, got %s", F, expected, actual)
		}

		_ = components.Next().Unwrap()
		expected = "Components([Normal(\"tmp\")])"
		if actual := components.DebugString(); expected != actual {
			t.Errorf("%v: expected %s, got %s", F, expected, actual)
		}

		_ = components.Next().Unwrap()
		expected = "Components([])"
		if actual := components.DebugString(); expected != actual {
			t.Errorf("%v: expected %s, got %s", F, expected, actual)
		}
	}
}

// The same test through the formatter: `format!("{components:?}")`.
func TestItPath_test_components_debug_formatted(t *testing.T) {
	if !formatterReady() {
		t.Skip("rs.Format is not ported yet; the text is checked through DebugString above")
	}
	for _, F := range bothFlavors {
		components := F.Components("/tmp")
		for _, expected := range []string{
			"Components([RootDir, Normal(\"tmp\")])", "Components([Normal(\"tmp\")])", "Components([])",
		} {
			if actual := rs.Format("{components:?}", rs.Arg("components", components)); expected != actual {
				t.Errorf("%v: expected %s, got %s", F, expected, actual)
			}
			components.Next()
		}
	}
}

func TestItPath_test_iter_debug(t *testing.T) {
	// #[cfg(any(unix, target_os = "wasi"))]
	F := Unix
	path := "/tmp"

	iter := F.Iter(path)

	expected := "Iter([\"/\", \"tmp\"])"
	if actual := iter.DebugString(); expected != actual {
		t.Errorf("expected %s, got %s", expected, actual)
	}

	_ = iter.Next().Unwrap()
	expected = "Iter([\"tmp\"])"
	if actual := iter.DebugString(); expected != actual {
		t.Errorf("expected %s, got %s", expected, actual)
	}

	_ = iter.Next().Unwrap()
	expected = "Iter([])"
	if actual := iter.DebugString(); expected != actual {
		t.Errorf("expected %s, got %s", expected, actual)
	}
}

// The same test through the formatter: `format!("{iter:?}")`.
func TestItPath_test_iter_debug_formatted(t *testing.T) {
	if !formatterReady() {
		t.Skip("rs.Format is not ported yet; the text is checked through DebugString above")
	}
	iter := Unix.Iter("/tmp")
	for _, expected := range []string{"Iter([\"/\", \"tmp\"])", "Iter([\"tmp\"])", "Iter([])"} {
		if actual := rs.Format("{iter:?}", rs.Arg("iter", iter)); expected != actual {
			t.Errorf("expected %s, got %s", expected, actual)
		}
		iter.Next()
	}
}

func TestItPath_into_boxed(t *testing.T) {
	for _, F := range bothFlavors {
		orig := "some/sort/of/path"
		path := orig
		boxed := path
		pathBuf := path
		if !F.Eq(path, boxed) || !F.Eq(boxed, pathBuf) || !F.Eq(pathBuf, path) {
			t.Errorf("%v: a path is not equal to itself", F)
		}
	}
}

func TestItPath_test_clone_into(t *testing.T) {
	for _, F := range bothFlavors {
		pathBuf := "supercalifragilisticexpialidocious"
		path := "short"
		pathBuf = path
		if !F.Eq(path, pathBuf) {
			t.Errorf("%v: %q != %q", F, path, pathBuf)
		}
		// The capacity assertion has no counterpart: a Go string has none.
	}
}

// displayFormatted is `format!(format, path.display())`.
func displayFormatted(F Flavor, format, path string) string {
	return rs.Format(format, F.Display(path))
}

func TestItPath_display_format_flags(t *testing.T) {
	if !formatterReady() {
		t.Skip("rs.Format is not ported yet")
	}
	for _, F := range bothFlavors {
		if got := displayFormatted(F, "a{:#<5}b", ""); got != "a#####b" {
			t.Errorf("%v: %q", F, got)
		}
		if got := displayFormatted(F, "a{:#<5}b", "a"); got != "aa####b" {
			t.Errorf("%v: %q", F, got)
		}
	}
}

func TestItPath_display_path_with_padding_no_align(t *testing.T) {
	if !formatterReady() {
		t.Skip("rs.Format is not ported yet")
	}
	for _, F := range bothFlavors {
		if got := displayFormatted(F, "{:10}", "/foo/bar"); got != "/foo/bar  " {
			t.Errorf("%v: %q", F, got)
		}
	}
}

func TestItPath_display_path_with_padding_align_left(t *testing.T) {
	if !formatterReady() {
		t.Skip("rs.Format is not ported yet")
	}
	for _, F := range bothFlavors {
		if got := displayFormatted(F, "{:<10}", "/foo/bar"); got != "/foo/bar  " {
			t.Errorf("%v: %q", F, got)
		}
	}
}

func TestItPath_display_path_with_padding_align_right(t *testing.T) {
	if !formatterReady() {
		t.Skip("rs.Format is not ported yet")
	}
	for _, F := range bothFlavors {
		if got := displayFormatted(F, "{:>10}", "/foo/bar"); got != "  /foo/bar" {
			t.Errorf("%v: %q", F, got)
		}
	}
}

func TestItPath_display_path_with_padding_align_center(t *testing.T) {
	if !formatterReady() {
		t.Skip("rs.Format is not ported yet")
	}
	for _, F := range bothFlavors {
		if got := displayFormatted(F, "{:^10}", "/foo/bar"); got != " /foo/bar " {
			t.Errorf("%v: %q", F, got)
		}
	}
}

func TestItPath_into_rc(t *testing.T) {
	for _, F := range bothFlavors {
		orig := "hello/world"
		path := orig
		rc := path
		arc := path

		if !F.Eq(rc, path) || !F.Eq(arc, path) {
			t.Errorf("%v: a path is not equal to itself", F)
		}

		rc2 := path
		arc2 := path

		if !F.Eq(rc2, path) || !F.Eq(arc2, path) {
			t.Errorf("%v: a path is not equal to itself", F)
		}
	}
}

func TestItPath_test_ord(t *testing.T) {
	const (
		Less    = -1
		Equal   = 0
		Greater = 1
	)
	_ = Greater

	ord := func(F Flavor, ord int, left, right string) {
		t.Helper()
		if got := F.Cmp(left, right); got != ord {
			t.Errorf("%v: %q.cmp(%q): expected %d, got %d", F, left, right, ord, got)
		}
		if ord == Equal {
			if !F.Eq(left, right) {
				t.Errorf("%v: %q != %q", F, left, right)
			}

			// The hash is the port's HashKey.
			leftHash := F.HashKey(left)
			rightHash := F.HashKey(right)

			if leftHash != rightHash {
				t.Errorf("%v: hashes for %q and %q must match", F, left, right)
			}
		} else {
			if F.Eq(left, right) {
				t.Errorf("%v: %q == %q", F, left, right)
			}
		}
	}

	for _, F := range bothFlavors {
		ord(F, Less, "1", "2")
		ord(F, Less, "/foo/bar", "/foo./bar")
		ord(F, Less, "foo/bar", "foo/bar.")
		ord(F, Equal, "foo/./bar", "foo/bar/")
		ord(F, Equal, "foo/bar", "foo/bar/")
		ord(F, Equal, "foo/bar", "foo/bar/.")
		ord(F, Equal, "foo/bar", "foo/bar//")
	}
}

func TestItPath_test_extension_path_sep(t *testing.T) {
	// #[should_panic = "path separator"]
	for _, F := range bothFlavors {
		shouldPanic(t, "path separator", func() {
			path := "path/to/file"
			F.SetExtension(&path, "d/../../../../../etc/passwd")
		})
	}
}

func TestItPath_test_extension_path_sep_alternate(t *testing.T) {
	// #[cfg(windows)]
	// #[should_panic = "path separator"]
	shouldPanic(t, "path separator", func() {
		path := "path/to/file"
		Windows.SetExtension(&path, "d\\test")
	})

	// #[cfg(not(windows))]
	path := "path/to/file"
	Unix.SetExtension(&path, "d\\test")
	if !Unix.Eq(path, "path/to/file.d\\test") {
		t.Errorf("%q", path)
	}
}

// shouldPanic is #[should_panic = "expected"]: run must panic with a message
// that contains expected.
func shouldPanic(t *testing.T, expected string, run func()) {
	t.Helper()
	defer func() {
		t.Helper()
		recovered := recover()
		if recovered == nil {
			t.Errorf("did not panic; expected a panic containing %q", expected)
			return
		}
		if message := rs.PanicInfoOf(recovered).Message; !strings.Contains(message, expected) {
			t.Errorf("panicked with %q; expected a panic containing %q", message, expected)
		}
	}()
	run()
}

func TestItPath_clone_to_uninit(t *testing.T) {
	// Cloning into uninitialized memory has no counterpart. What is left:
	// the bytes of a path are its string, and two different paths differ.
	for _, F := range bothFlavors {
		a := "hello.txt"
		if got := string(OsStrAsEncodedBytes(a)); got != "hello.txt" {
			t.Errorf("as_encoded_bytes: %q", got)
		}

		b := "world.exe"
		if F.Eq(a, b) {
			t.Errorf("%v: %q == %q", F, a, b)
		}
		b = a
		if !F.Eq(a, b) {
			t.Errorf("%v: %q != %q", F, a, b)
		}
	}
}

// Test: Only separators (e.g., "/" or "\\")
// This test checks how Path handles a string that consists only of path separators.
// It should recognize the root and not treat it as a normal component.
func TestItPath_test_only_separators(t *testing.T) {
	for _, F := range bothFlavors {
		path := "/////"
		if !F.HasRoot(path) {
			t.Errorf("%v: has_root", F)
		}
		if count := len(F.Iter(path).Collect()); count != 1 {
			t.Errorf("%v: iter().count() = %d", F, count)
		}
		if F.Parent(path) != none {
			t.Errorf("%v: parent = %v", F, debugOption(F.Parent(path)))
		}
	}
}

// Test: Non-ASCII/Unicode
// This test verifies that Path can handle Unicode and non-ASCII characters in the path.
// It ensures that such paths are not rejected or misinterpreted.
func TestItPath_test_non_ascii_unicode(t *testing.T) {
	for _, F := range bothFlavors {
		path := "/tmp/❤/🚀/file.txt"
		if !F.ToStr(path).IsSome() {
			t.Errorf("%v: to_str is None", F)
		}
		if F.FileName(path) != some("file.txt") {
			t.Errorf("%v: file_name = %v", F, debugOption(F.FileName(path)))
		}
	}
}

// Test: Embedded newlines
// This test verifies that newlines within path components are preserved and do not break path parsing.
// It ensures that Path treats newlines as normal characters.
func TestItPath_test_embedded_newline(t *testing.T) {
	for _, F := range bothFlavors {
		path := "foo\nbar"
		if F.FileName(path) != some("foo\nbar") {
			t.Errorf("%v: file_name = %v", F, debugOption(F.FileName(path)))
		}
		if F.ToStr(path) != some("foo\nbar") {
			t.Errorf("%v: to_str = %v", F, debugOption(F.ToStr(path)))
		}
	}
}

func TestItPath_normalize_lexically(t *testing.T) {
	checkOk := func(F Flavor, a, b string) {
		t.Helper()
		got, err := F.NormalizeLexically(a)
		if err != nil {
			t.Errorf("%v: normalize_lexically(%q): %v", F, a, err)
			return
		}
		if !F.Eq(got, b) {
			t.Errorf("%v: normalize_lexically(%q) = %q, expected %q", F, a, got, b)
		}
	}

	checkErr := func(F Flavor, a string) {
		t.Helper()
		if got, err := F.NormalizeLexically(a); err == nil {
			t.Errorf("%v: normalize_lexically(%q) = %q, expected an error", F, a, got)
		}
	}

	for _, F := range bothFlavors {
		// Relative paths
		checkOk(F, "a", "a")
		checkOk(F, "./a", "./a")
		checkOk(F, "a/b/c", "a/b/c")
		checkOk(F, "a/././b/./c/.", "a/b/c")
		checkOk(F, "a/../c", "c")
		checkOk(F, "./a/b", "./a/b")
		checkOk(F, "a/../b/c/..", "b")

		checkErr(F, "..")
		checkErr(F, "../..")
		checkErr(F, "a/../..")
		checkErr(F, "a/../../b")
		checkErr(F, "a/../../b/c")
		checkErr(F, "a/../b/../..")
	}

	// Check we don't escape the root or prefix
	// #[cfg(unix)]
	{
		F := Unix
		checkErr(F, "/..")
		checkErr(F, "/a/../..")
	}
	// #[cfg(windows)]
	{
		F := Windows
		checkErr(F, `C:\..`)
		checkErr(F, `C:\a\..\..`)

		checkErr(F, `C:..`)
		checkErr(F, `C:a\..\..`)

		checkErr(F, `\\server\share\..`)
		checkErr(F, `\\server\share\a\..\..`)

		checkErr(F, `\..`)
		checkErr(F, `\a\..\..`)

		checkErr(F, `\\?\UNC\server\share\..`)
		checkErr(F, `\\?\UNC\server\share\a\..\..`)
	}
}

// See issue#146183 and issue#146940
func TestItPath_compare_path_like_to_str_like(t *testing.T) {
	// Path == str, str == Path, PathBuf == String and the rest are all the
	// comparison of two paths: every operand is a string here.
	for _, F := range bothFlavors {
		pathBuf := "x"
		path := "x"
		s := "x"
		for _, pair := range [][2]string{
			{path, "x"}, {"x", path}, {path, s}, {s, path}, {pathBuf, "x"}, {"x", pathBuf}, {pathBuf, s}, {s, pathBuf},
		} {
			if !F.Eq(pair[0], pair[1]) {
				t.Errorf("%v: %q != %q", F, pair[0], pair[1])
			}
		}
	}
}

func TestItPath_test_trim_trailing_sep(t *testing.T) {
	check := func(F Flavor, path, expected string) {
		t.Helper()
		if got := F.TrimTrailingSep(path); got != expected {
			t.Errorf("%v: trim_trailing_sep(%q) = %q, expected %q", F, path, got, expected)
		}
	}

	for _, F := range bothFlavors {
		check(F, "/", "/")
		check(F, "//", "//")
		check(F, "", "")
		check(F, ".", ".")
		check(F, "./", ".")
		check(F, ".//", ".")
		check(F, "..", "..")
		check(F, "../", "..")
		check(F, "..//", "..")
	}

	// #[cfg(any(windows, target_os = "cygwin"))]
	{
		F := Windows
		check(F, "\\", "\\")
		check(F, "\\\\", "\\\\")
		check(F, "c:/", "c:/")
		check(F, "c://", "c://")
		check(F, "c:./", "c:.")
		check(F, "c:.//", "c:.")
		check(F, "c:../", "c:..")
		check(F, "c:..//", "c:..")
		check(F, "c:\\", "c:\\")
		check(F, "c:\\\\", "c:\\\\")
		check(F, "c:.\\", "c:.")
		check(F, "c:.\\\\", "c:.")
		check(F, "c:..\\", "c:..")
		check(F, "c:..\\\\", "c:..")
	}
}
