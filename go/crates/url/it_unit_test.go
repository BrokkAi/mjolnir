// Port of url 2.5.8 tests/unit.rs

package url

import (
	"net/netip"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Unit tests
//
// The tests that Rust compiles only for Unix or only for Windows run here on
// every system: urlFromFilePath, urlFromDirectoryPath and toFilePath take the
// platform as an argument (windows), which is what `cfg!(windows)` decides in
// Rust.

// itUnitAssertEq is `assert_eq!` for values Go compares as Rust does.
func itUnitAssertEq[T comparable](t *testing.T, left T, right T, message ...any) {
	t.Helper()
	if left != right {
		text := "assertion `left == right` failed"
		if len(message) > 0 {
			text += ": " + rs.Format(message[0].(string), message[1:]...)
		}
		t.Fatalf("%s\n  left: %s\n right: %s", text, rs.DebugString(left), rs.DebugString(right))
	}
}

// itUnitAssertNe is `assert_ne!`.
func itUnitAssertNe[T comparable](t *testing.T, left T, right T) {
	t.Helper()
	if left == right {
		t.Fatalf("assertion `left != right` failed\n  left: %s\n right: %s", rs.DebugString(left), rs.DebugString(right))
	}
}

// itUnitAssert is `assert!`.
func itUnitAssert(t *testing.T, cond bool, message ...any) {
	t.Helper()
	if !cond {
		if len(message) > 0 {
			t.Fatal(rs.Format(message[0].(string), message[1:]...))
		}
		t.Fatal("assertion failed")
	}
}

// itUnitParse is `s.parse::<Url>().unwrap()`.
func itUnitParse(t *testing.T, s string) Url {
	t.Helper()
	url, err := UrlFromStr(s)
	if err != nil {
		t.Fatalf("called `Result::unwrap()` on an `Err` value: %s", rs.DebugString(err))
	}
	return url
}

// itUnitOk is `.unwrap()` on a `Result<(), _>`.
func itUnitOk(t *testing.T, err error) {
	t.Helper()
	if err != nil {
		t.Fatalf("called `Result::unwrap()` on an `Err` value: %s", rs.DebugString(err))
	}
}

// itUnitAssertHost is `assert_eq!(url.host(), Some(host))` and, with no
// host, `assert_eq!(url.host(), None)`.
func itUnitAssertHost(t *testing.T, got rs.Option[Host], want ...Host) {
	t.Helper()
	host, ok := got.Get()
	switch {
	case len(want) == 0 && ok:
		t.Fatalf("assertion `left == right` failed\n  left: Some(%s)\n right: None", rs.DebugString(host))
	case len(want) == 1 && !ok:
		t.Fatalf("assertion `left == right` failed\n  left: None\n right: Some(%s)", rs.DebugString(want[0]))
	case len(want) == 1 && !host.Equal(want[0]):
		t.Fatalf("assertion `left == right` failed\n  left: Some(%s)\n right: Some(%s)", rs.DebugString(host), rs.DebugString(want[0]))
	}
}

func TestItUnit_size(t *testing.T) {
	t.Skip("size_of::<Url>() == size_of::<Option<Url>>() is Rust's niche layout, which Go has no counterpart of")
}

func TestItUnit_test_relative(t *testing.T) {
	base := itUnitParse(t, "sc://%C3%B1")
	url, err := base.Join("/resources/testharness.js")
	itUnitOk(t, err)
	itUnitAssertEq(t, url.AsStr(), "sc://%C3%B1/resources/testharness.js")
}

func TestItUnit_test_relative_empty(t *testing.T) {
	base := itUnitParse(t, "sc://%C3%B1")
	url, err := base.Join("")
	itUnitOk(t, err)
	itUnitAssertEq(t, url.AsStr(), "sc://%C3%B1")
}

func TestItUnit_test_strip_trailing_spaces_from_opaque_path(t *testing.T) {
	url := itUnitParse(t, "data:space   ?query")
	url.SetQuery(rs.None[string]())
	itUnitAssertEq(t, url.AsStr(), "data:space")

	url = itUnitParse(t, "data:space   #hash")
	url.SetFragment(rs.None[string]())
	itUnitAssertEq(t, url.AsStr(), "data:space")
}

func TestItUnit_test_set_empty_host(t *testing.T) {
	base := itUnitParse(t, "moz://foo:bar@servo/baz")
	itUnitOk(t, base.SetUsername(""))
	itUnitAssertEq(t, base.AsStr(), "moz://:bar@servo/baz")
	itUnitOk(t, base.SetHost(rs.None[string]()))
	itUnitAssertEq(t, base.AsStr(), "moz:/baz")
	itUnitOk(t, base.SetHost(rs.Some("servo")))
	itUnitAssertEq(t, base.AsStr(), "moz://servo/baz")

	base = itUnitParse(t, "file://server/share/foo/bar")
	itUnitOk(t, base.SetHost(rs.None[string]()))
	itUnitAssertEq(t, base.AsStr(), "file:///share/foo/bar")

	base = itUnitParse(t, "file://server/share/foo/bar")
	itUnitOk(t, base.SetHost(rs.Some("foo")))
	itUnitAssertEq(t, base.AsStr(), "file://foo/share/foo/bar")
}

func TestItUnit_test_set_empty_username_and_password(t *testing.T) {
	base := itUnitParse(t, "moz://foo:bar@servo/baz")
	itUnitOk(t, base.SetUsername(""))
	itUnitAssertEq(t, base.AsStr(), "moz://:bar@servo/baz")

	itUnitOk(t, base.SetPassword(rs.Some("")))
	itUnitAssertEq(t, base.AsStr(), "moz://servo/baz")

	itUnitOk(t, base.SetPassword(rs.None[string]()))
	itUnitAssertEq(t, base.AsStr(), "moz://servo/baz")
}

func TestItUnit_test_set_empty_password(t *testing.T) {
	base := itUnitParse(t, "moz://foo:bar@servo/baz")

	itUnitOk(t, base.SetPassword(rs.Some("")))
	itUnitAssertEq(t, base.AsStr(), "moz://foo@servo/baz")

	itUnitOk(t, base.SetPassword(rs.None[string]()))
	itUnitAssertEq(t, base.AsStr(), "moz://foo@servo/baz")
}

func TestItUnit_test_set_empty_hostname(t *testing.T) {
	base := itUnitParse(t, "moz://foo@servo/baz")
	itUnitAssert(t,
		QuirksSetHostname(&base, "") != nil,
		"setting an empty hostname to a url with a username should fail",
	)
	base = itUnitParse(t, "moz://:pass@servo/baz")
	itUnitAssert(t,
		QuirksSetHostname(&base, "") != nil,
		"setting an empty hostname to a url with a password should fail",
	)
	base = itUnitParse(t, "moz://servo/baz")
	itUnitOk(t, QuirksSetHostname(&base, ""))
	itUnitAssertEq(t, base.AsStr(), "moz:///baz")
}

func TestItUnit_test_set_empty_query(t *testing.T) {
	base := itUnitParse(t, "moz://example.com/path?query")

	base.SetQuery(rs.Some(""))
	itUnitAssertEq(t, base.AsStr(), "moz://example.com/path?")

	base.SetQuery(rs.None[string]())
	itUnitAssertEq(t, base.AsStr(), "moz://example.com/path")
}

// itUnitAssertFromFilePath is the macro assert_from_file_path!.
func itUnitAssertFromFilePath(t *testing.T, windows bool, path string, urlPath string) {
	t.Helper()
	url, err := urlFromFilePath(path, windows)
	itUnitOk(t, err)
	itUnitAssertHost(t, url.Host())
	itUnitAssertEq(t, url.Path(), urlPath)
	back, err := url.toFilePath(windows)
	itUnitOk(t, err)
	itUnitAssertEq(t, back, path)
}

func itUnitFromFilePathFails(t *testing.T, windows bool, path string) {
	t.Helper()
	_, err := urlFromFilePath(path, windows)
	itUnitAssertEq(t, err, errUnit)
}

func itUnitFromDirectoryPathFails(t *testing.T, windows bool, path string) {
	t.Helper()
	_, err := urlFromDirectoryPath(path, windows)
	itUnitAssertEq(t, err, errUnit)
}

func TestItUnit_new_file_paths(t *testing.T) {
	// cfg!(unix)
	itUnitFromFilePathFails(t, false, "relative")
	itUnitFromFilePathFails(t, false, "../relative")
	// cfg!(windows)
	itUnitFromFilePathFails(t, true, "relative")
	itUnitFromFilePathFails(t, true, `..\relative`)
	itUnitFromFilePathFails(t, true, `\drive-relative`)
	itUnitFromFilePathFails(t, true, `\\ucn\`)

	// cfg!(unix)
	itUnitAssertFromFilePath(t, false, "/foo/bar", "/foo/bar")
	itUnitAssertFromFilePath(t, false, "/foo/ba\x00r", "/foo/ba%00r")
	itUnitAssertFromFilePath(t, false, "/foo/ba%00r", "/foo/ba%2500r")
	itUnitAssertFromFilePath(t, false, "/foo/ba\\r", "/foo/ba%5Cr")
}

func TestItUnit_new_path_bad_utf8(t *testing.T) {
	url, err := urlFromFilePath("/foo/ba\x80r", false)
	itUnitOk(t, err)
	osStr := "/foo/ba\x80r"
	path, err := url.toFilePath(false)
	itUnitOk(t, err)
	itUnitAssertEq(t, path, osStr)
}

func TestItUnit_new_path_windows_fun(t *testing.T) {
	itUnitAssertFromFilePath(t, true, `C:\foo\bar`, "/C:/foo/bar")
	itUnitAssertFromFilePath(t, true, "C:\\foo\\ba\x00r", "/C:/foo/ba%00r")

	// Invalid UTF-8
	url := itUnitParse(t, "file:///C:/foo/ba%80r")
	_, err := url.toFilePath(true)
	itUnitAssert(t, err != nil)

	// test windows canonicalized path
	path := `\\?\C:\foo\bar`
	_, err = urlFromFilePath(path, true)
	itUnitAssert(t, err == nil)

	// Percent-encoded drive letter
	url = itUnitParse(t, "file:///C%3A/foo/bar")
	back, err := url.toFilePath(true)
	itUnitOk(t, err)
	itUnitAssertEq(t, back, `C:\foo\bar`)
}

func TestItUnit_new_directory_paths(t *testing.T) {
	// cfg!(unix)
	itUnitFromDirectoryPathFails(t, false, "relative")
	itUnitFromDirectoryPathFails(t, false, "../relative")

	url, err := urlFromDirectoryPath("/foo/bar", false)
	itUnitOk(t, err)
	itUnitAssertHost(t, url.Host())
	itUnitAssertEq(t, url.Path(), "/foo/bar/")

	// cfg!(windows)
	itUnitFromDirectoryPathFails(t, true, "relative")
	itUnitFromDirectoryPathFails(t, true, `..\relative`)
	itUnitFromDirectoryPathFails(t, true, `\drive-relative`)
	itUnitFromDirectoryPathFails(t, true, `\\ucn\`)

	url, err = urlFromDirectoryPath(`C:\foo\bar`, true)
	itUnitOk(t, err)
	itUnitAssertHost(t, url.Host())
	itUnitAssertEq(t, url.Path(), "/C:/foo/bar/")
}

func TestItUnit_path_backslash_fun(t *testing.T) {
	specialUrl := itUnitParse(t, "http://foobar.com")
	segments, err := specialUrl.PathSegmentsMut()
	itUnitOk(t, err)
	segments.Push("foo\\bar")
	segments.Drop()
	itUnitAssertEq(t, specialUrl.AsStr(), "http://foobar.com/foo%5Cbar")

	nonspecialUrl := itUnitParse(t, "thing://foobar.com")
	segments, err = nonspecialUrl.PathSegmentsMut()
	itUnitOk(t, err)
	segments.Push("foo\\bar")
	segments.Drop()
	itUnitAssertEq(t, nonspecialUrl.AsStr(), "thing://foobar.com/foo\\bar")
}

func TestItUnit_from_str(t *testing.T) {
	_, err := UrlFromStr("http://testing.com/this")
	itUnitAssert(t, err == nil)
}

func TestItUnit_parse_with_params(t *testing.T) {
	url, err := UrlParseWithParams(
		"http://testing.com/this?dont=clobberme",
		[]rs.Tuple2[string, string]{rs.T2("lang", "rust")},
	)
	itUnitOk(t, err)

	itUnitAssertEq(t,
		url.AsStr(),
		"http://testing.com/this?dont=clobberme&lang=rust",
	)
}

func TestItUnit_issue_124(t *testing.T) {
	url := itUnitParse(t, "file:a")
	itUnitAssertEq(t, url.Path(), "/a")
	url = itUnitParse(t, "file:...")
	itUnitAssertEq(t, url.Path(), "/...")
	url = itUnitParse(t, "file:..")
	itUnitAssertEq(t, url.Path(), "/")
}

func TestItUnit_test_equality(t *testing.T) {
	// The hash is `impl Hash for Url`, whose Go form is HashKey.
	checkEq := func(a *Url, b *Url) {
		t.Helper()
		itUnitAssert(t, a.Equal(b), "assertion `left == right` failed\n  left: {:?}\n right: {:?}", a, b)
		itUnitAssertEq(t, a.HashKey(), b.HashKey())
	}

	url := func(s string) Url {
		t.Helper()
		rv := itUnitParse(t, s)
		checkEq(&rv, &rv)
		return rv
	}

	// Doesn't care if default port is given.
	a := url("https://example.com/")
	b := url("https://example.com:443/")
	checkEq(&a, &b)

	// Different ports
	a = url("http://example.com/")
	b = url("http://example.com:8080/")
	itUnitAssert(t, !a.Equal(&b), "{:?} != {:?}", &a, &b)

	// Different scheme
	a = url("http://example.com/")
	b = url("https://example.com/")
	itUnitAssert(t, !a.Equal(&b))

	// Different host
	a = url("http://foo.com/")
	b = url("http://bar.com/")
	itUnitAssert(t, !a.Equal(&b))

	// Missing path, automatically substituted. Semantically the same.
	a = url("http://foo.com")
	b = url("http://foo.com/")
	checkEq(&a, &b)
}

func TestItUnit_host(t *testing.T) {
	assertHost := func(input string, host Host) {
		t.Helper()
		url, err := UrlParse(input)
		itUnitOk(t, err)
		itUnitAssertHost(t, url.Host(), host)
	}
	ipv4 := func(a, b, c, d byte) Host { return &Host_Ipv4{F0: netip.AddrFrom4([4]byte{a, b, c, d})} }
	ipv6 := func(segments ...uint16) Host {
		var bytes [16]byte
		for i, segment := range segments {
			bytes[2*i] = byte(segment >> 8)
			bytes[2*i+1] = byte(segment)
		}
		return &Host_Ipv6{F0: netip.AddrFrom16(bytes)}
	}
	assertHost("http://www.mozilla.org", &Host_Domain{F0: "www.mozilla.org"})
	assertHost(
		"http://1.35.33.49",
		ipv4(1, 35, 33, 49),
	)
	assertHost(
		"http://[2001:0db8:85a3:08d3:1319:8a2e:0370:7344]",
		ipv6(
			0x2001, 0x0db8, 0x85a3, 0x08d3, 0x1319, 0x8a2e, 0x0370, 0x7344,
		),
	)
	assertHost(
		"http://[::]",
		ipv6(0, 0, 0, 0, 0, 0, 0, 0),
	)
	assertHost(
		"http://[::1]",
		ipv6(0, 0, 0, 0, 0, 0, 0, 1),
	)
	assertHost(
		"http://0x1.0X23.0x21.061",
		ipv4(1, 35, 33, 49),
	)
	assertHost("http://0x1232131", ipv4(1, 35, 33, 49))
	assertHost("http://111", ipv4(0, 0, 0, 111))
	for _, input := range []string{"http://1.35.+33.49", "http://2..2.3", "http://42.0x1232131", "http://192.168.0.257"} {
		_, err := UrlParse(input)
		itUnitAssert(t, err != nil)
	}

	foo := &Host_Domain{F0: "foo"}
	itUnitAssert(t, foo.Equal((&Host_Domain{F0: "foo"}).ToOwned()))
	itUnitAssert(t, !foo.Equal((&Host_Domain{F0: "bar"}).ToOwned()))
}

func TestItUnit_host_serialization(t *testing.T) {
	// libstd’s `Display for Ipv6Addr` serializes 0:0:0:0:0:0:_:_ and 0:0:0:0:0:ffff:_:_
	// using IPv4-like syntax, as suggested in https://tools.ietf.org/html/rfc5952#section-4
	// but https://url.spec.whatwg.org/#concept-ipv6-serializer specifies not to.

	// Not [::0.0.0.2] / [::ffff:0.0.0.2]
	url, err := UrlParse("http://[0::2]")
	itUnitOk(t, err)
	itUnitAssertEq(t, url.HostStr(), rs.Some("[::2]"))
	url, err = UrlParse("http://[0::ffff:0:2]")
	itUnitOk(t, err)
	itUnitAssertEq(t, url.HostStr(), rs.Some("[::ffff:0:2]"))
}

func TestItUnit_test_idna(t *testing.T) {
	_, err := UrlFromStr("http://goșu.ro")
	itUnitAssert(t, err == nil)
	url, err := UrlParse("http://☃.net/")
	itUnitOk(t, err)
	itUnitAssertHost(t, url.Host(), &Host_Domain{F0: "xn--n3h.net"})
	_, err = UrlFromStr("https://r2---sn-huoa-cvhl.googlevideo.com/crossdomain.xml")
	itUnitAssert(t, err == nil)
}

func TestItUnit_test_serialization(t *testing.T) {
	data := [][2]string{
		{"http://example.com/", "http://example.com/"},
		{"http://addslash.com", "http://addslash.com/"},
		{"http://@emptyuser.com/", "http://emptyuser.com/"},
		{"http://:@emptypass.com/", "http://emptypass.com/"},
		{"http://user@user.com/", "http://user@user.com/"},
		{
			"http://user:pass@userpass.com/",
			"http://user:pass@userpass.com/",
		},
		{
			"http://slashquery.com/path/?q=something",
			"http://slashquery.com/path/?q=something",
		},
		{
			"http://noslashquery.com/path?q=something",
			"http://noslashquery.com/path?q=something",
		},
	}
	for _, pair := range data {
		input, result := pair[0], pair[1]
		url, err := UrlParse(input)
		itUnitOk(t, err)
		itUnitAssertEq(t, url.AsStr(), result)
	}
}

func TestItUnit_test_form_urlencoded(t *testing.T) {
	pairs := []rs.Tuple2[string, string]{
		rs.T2("foo", "é&"),
		rs.T2("bar", ""),
		rs.T2("foo", "#"),
	}
	encoded := FormUrlencodedSerializerNew(FormUrlencodedTargetString("")).
		ExtendPairs(pairs).
		Finish()
	itUnitAssertEq(t, encoded, "foo=%C3%A9%26&bar=&foo=%23")
	decoded := FormUrlencodedParse([]byte(encoded)).Collect()
	itUnitAssertEq(t, len(decoded), len(pairs))
	for i := range pairs {
		itUnitAssertEq(t, decoded[i], pairs[i])
	}
}

func TestItUnit_test_form_serialize(t *testing.T) {
	encoded := FormUrlencodedSerializerNew(FormUrlencodedTargetString("")).
		AppendPair("foo", "é&").
		AppendPair("bar", "").
		AppendPair("foo", "#").
		AppendKeyOnly("json").
		Finish()
	itUnitAssertEq(t, encoded, "foo=%C3%A9%26&bar=&foo=%23&json")
}

func TestItUnit_form_urlencoded_encoding_override(t *testing.T) {
	encoded := FormUrlencodedSerializerNew(FormUrlencodedTargetString("")).
		EncodingOverride(rs.Some(func(s string) []byte {
			upper := []byte(s)
			for i, c := range upper {
				if 'a' <= c && c <= 'z' {
					upper[i] = c - ('a' - 'A')
				}
			}
			return upper
		})).
		AppendPair("foo", "bar").
		AppendKeyOnly("xml").
		Finish()
	itUnitAssertEq(t, encoded, "FOO=BAR&XML")
}

// https://github.com/servo/rust-url/issues/61
func TestItUnit_issue_61(t *testing.T) {
	url, err := UrlParse("http://mozilla.org")
	itUnitOk(t, err)
	itUnitOk(t, url.SetScheme("https"))
	itUnitAssertEq(t, url.Port(), rs.None[uint16]())
	itUnitAssertEq(t, url.PortOrKnownDefault(), rs.Some[uint16](443))
	itUnitOk(t, url.CheckInvariants())
}

// https://github.com/servo/rust-url/issues/197
func TestItUnit_issue_197(t *testing.T) {
	url, err := urlFromFilePath("/", false)
	if err != nil {
		t.Fatalf("Failed to parse path: %s", rs.DebugString(err))
	}
	itUnitOk(t, url.CheckInvariants())
	other, err := UrlParse("file:///")
	if err != nil {
		t.Fatalf("Failed to parse path + protocol: %s", rs.DebugString(err))
	}
	itUnitAssert(t, url.Equal(&other))
	segments, err := url.PathSegmentsMut()
	if err != nil {
		t.Fatalf("path_segments_mut: %s", rs.DebugString(err))
	}
	segments.PopIfEmpty()
	segments.Drop()
}

func TestItUnit_issue_241(t *testing.T) {
	url, err := UrlParse("mailto:")
	itUnitOk(t, err)
	url.CannotBeABase()
}

// https://github.com/servo/rust-url/issues/222
func TestItUnit_append_trailing_slash(t *testing.T) {
	url := itUnitParse(t, "http://localhost:6767/foo/bar?a=b")
	itUnitOk(t, url.CheckInvariants())
	segments, err := url.PathSegmentsMut()
	itUnitOk(t, err)
	segments.Push("")
	segments.Drop()
	itUnitOk(t, url.CheckInvariants())
	itUnitAssertEq(t, url.String(), "http://localhost:6767/foo/bar/?a=b")
}

// https://github.com/servo/rust-url/issues/227
func TestItUnit_extend_query_pairs_then_mutate(t *testing.T) {
	url := itUnitParse(t, "http://localhost:6767/foo/bar")
	pairs := url.QueryPairsMut()
	pairs.ExtendPairs([]rs.Tuple2[string, string]{rs.T2("auth", "my-token")})
	pairs.Drop()
	itUnitOk(t, url.CheckInvariants())
	itUnitAssertEq(t,
		url.String(),
		"http://localhost:6767/foo/bar?auth=my-token",
	)
	segments, err := url.PathSegmentsMut()
	itUnitOk(t, err)
	segments.Push("some_other_path")
	segments.Drop()
	itUnitOk(t, url.CheckInvariants())
	itUnitAssertEq(t,
		url.String(),
		"http://localhost:6767/foo/bar/some_other_path?auth=my-token",
	)
}

// https://github.com/servo/rust-url/issues/222
func TestItUnit_append_empty_segment_then_mutate(t *testing.T) {
	url := itUnitParse(t, "http://localhost:6767/foo/bar?a=b")
	itUnitOk(t, url.CheckInvariants())
	segments, err := url.PathSegmentsMut()
	itUnitOk(t, err)
	segments.Push("").Pop()
	segments.Drop()
	itUnitOk(t, url.CheckInvariants())
	itUnitAssertEq(t, url.String(), "http://localhost:6767/foo/bar?a=b")
}

// https://github.com/servo/rust-url/issues/243
func TestItUnit_test_set_host(t *testing.T) {
	url, err := UrlParse("https://example.net/hello")
	itUnitOk(t, err)
	itUnitOk(t, url.SetHost(rs.Some("foo.com")))
	itUnitAssertEq(t, url.AsStr(), "https://foo.com/hello")
	itUnitAssert(t, url.SetHost(rs.None[string]()) != nil)
	itUnitAssertEq(t, url.AsStr(), "https://foo.com/hello")
	itUnitAssert(t, url.SetHost(rs.Some("")) != nil)
	itUnitAssertEq(t, url.AsStr(), "https://foo.com/hello")

	url, err = UrlParse("foobar://example.net/hello")
	itUnitOk(t, err)
	itUnitOk(t, url.SetHost(rs.None[string]()))
	itUnitAssertEq(t, url.AsStr(), "foobar:/hello")

	url, err = UrlParse("foo://ș")
	itUnitOk(t, err)
	itUnitAssertEq(t, url.AsStr(), "foo://%C8%99")
	itUnitOk(t, url.SetHost(rs.Some("goșu.ro")))
	itUnitAssertEq(t, url.AsStr(), "foo://go%C8%99u.ro")
}

// https://github.com/servo/rust-url/issues/166
func TestItUnit_test_leading_dots(t *testing.T) {
	host, err := HostParse(".org")
	itUnitOk(t, err)
	itUnitAssert(t, host.Equal(&Host_Domain{F0: ".org"}))
	url, err := UrlParse("file://./foo")
	itUnitOk(t, err)
	itUnitAssertEq(t, url.Domain(), rs.Some("."))
}

// https://github.com/servo/rust-url/issues/302
func TestItUnit_test_origin_hash(t *testing.T) {
	// The hash is the derived Hash, whose Go form is HashKey: equal keys
	// are equal hashes.
	hash := func(origin Origin) string { return origin.HashKey() }
	origin := func(input string) Origin {
		t.Helper()
		url, err := UrlParse(input)
		itUnitOk(t, err)
		return url.Origin()
	}

	base := origin("http://example.net/")

	originsToCompare := []Origin{
		origin("http://example.net:80/"),
		origin("http://example.net:81/"),
		origin("http://example.net"),
		origin("http://example.net/hello"),
		origin("https://example.net"),
		origin("ftp://example.net"),
		origin("file://example.net"),
		origin("http://user@example.net/"),
		origin("http://user:pass@example.net/"),
	}

	for _, originToCompare := range originsToCompare {
		if base.Equal(originToCompare) {
			itUnitAssertEq(t, hash(base), hash(originToCompare))
		} else {
			itUnitAssertNe(t, hash(base), hash(originToCompare))
		}
	}

	opaqueOrigin := origin("file://example.net")
	sameOpaqueOrigin := origin("file://example.net")
	otherOpaqueOrigin := origin("file://other")

	itUnitAssertNe(t, hash(opaqueOrigin), hash(sameOpaqueOrigin))
	itUnitAssertNe(t, hash(opaqueOrigin), hash(otherOpaqueOrigin))
}

func TestItUnit_test_origin_blob_equality(t *testing.T) {
	url, err := UrlParse("http://example.net/")
	itUnitOk(t, err)
	origin := url.Origin()
	blob, err := UrlParse("blob:http://example.net/")
	itUnitOk(t, err)
	blobOrigin := blob.Origin()

	itUnitAssert(t, origin.Equal(blobOrigin))
}

func TestItUnit_test_origin_opaque(t *testing.T) {
	itUnitAssert(t, !OriginNewOpaque().IsTuple())
	url, err := UrlParse("blob:malformed//")
	itUnitOk(t, err)
	itUnitAssert(t, !url.Origin().IsTuple())
}

func TestItUnit_test_origin_unicode_serialization(t *testing.T) {
	data := [][2]string{
		{"http://😅.com", "http://😅.com"},
		{"ftp://😅:🙂@🙂.com", "ftp://🙂.com"},
		{"https://user@😅.com", "https://😅.com"},
		{"http://😅.🙂:40", "http://😅.🙂:40"},
	}
	for _, pair := range data {
		unicodeUrl, expectedSerialization := pair[0], pair[1]
		url, err := UrlParse(unicodeUrl)
		itUnitOk(t, err)
		itUnitAssertEq(t, url.Origin().UnicodeSerialization(), expectedSerialization)
	}

	for _, input := range []string{
		"http://example.net/",
		"http://example.net:80/",
		"http://example.net:81/",
		"http://example.net",
		"http://example.net/hello",
		"https://example.net",
		"ftp://example.net",
		"file://example.net",
		"http://user@example.net/",
		"http://user:pass@example.net/",
		"http://127.0.0.1",
	} {
		url, err := UrlParse(input)
		itUnitOk(t, err)
		asciiOrigin := url.Origin()
		itUnitAssertEq(t,
			asciiOrigin.AsciiSerialization(),
			asciiOrigin.UnicodeSerialization(),
		)
	}
}

func TestItUnit_test_socket_addrs(t *testing.T) {
	t.Skip("Url::socket_addrs is not covered: Mjolnir does not call it, and it resolves names through the system")
}

func TestItUnit_test_no_base_url(t *testing.T) {
	noBaseUrl, err := UrlParse("mailto:test@example.net")
	itUnitOk(t, err)

	itUnitAssert(t, noBaseUrl.CannotBeABase())
	itUnitAssert(t, noBaseUrl.PathSegments().IsNone())
	_, err = noBaseUrl.PathSegmentsMut()
	itUnitAssert(t, err != nil)
	itUnitAssert(t, noBaseUrl.SetHost(rs.Some("foo")) != nil)
	itUnitAssert(t, noBaseUrl.
		SetIpHost(netip.MustParseAddr("127.0.0.1")) != nil)

	noBaseUrl.SetPath("/foo")
	itUnitAssertEq(t, noBaseUrl.Path(), "%2Ffoo")
}

func TestItUnit_test_domain(t *testing.T) {
	url, err := UrlParse("https://127.0.0.1/")
	itUnitOk(t, err)
	itUnitAssertEq(t, url.Domain(), rs.None[string]())

	url, err = UrlParse("mailto:test@example.net")
	itUnitOk(t, err)
	itUnitAssertEq(t, url.Domain(), rs.None[string]())

	url, err = UrlParse("https://example.com/")
	itUnitOk(t, err)
	itUnitAssertEq(t, url.Domain(), rs.Some("example.com"))
}

func TestItUnit_test_query(t *testing.T) {
	url, err := UrlParse("https://example.com/products?page=2#fragment")
	itUnitOk(t, err)
	itUnitAssertEq(t, url.Query(), rs.Some("page=2"))
	pairs := url.QueryPairs()
	itUnitAssertEq(t,
		pairs.Next(),
		rs.Some(rs.T2("page", "2")),
	)

	url, err = UrlParse("https://example.com/products")
	itUnitOk(t, err)
	itUnitAssert(t, url.Query().IsNone())
	itUnitAssertEq(t, url.QueryPairs().Count(), 0)

	url, err = UrlParse("https://example.com/?country=español")
	itUnitOk(t, err)
	itUnitAssertEq(t, url.Query(), rs.Some("country=espa%C3%B1ol"))
	pairs = url.QueryPairs()
	itUnitAssertEq(t,
		pairs.Next(),
		rs.Some(rs.T2("country", "español")),
	)

	url, err = UrlParse("https://example.com/products?page=2&sort=desc")
	itUnitOk(t, err)
	itUnitAssertEq(t, url.Query(), rs.Some("page=2&sort=desc"))
	pairs = url.QueryPairs()
	itUnitAssertEq(t, pairs.Count(), 2)
	itUnitAssertEq(t,
		pairs.Next(),
		rs.Some(rs.T2("page", "2")),
	)
	itUnitAssertEq(t,
		pairs.Next(),
		rs.Some(rs.T2("sort", "desc")),
	)
}

func TestItUnit_test_fragment(t *testing.T) {
	url, err := UrlParse("https://example.com/#fragment")
	itUnitOk(t, err)
	itUnitAssertEq(t, url.Fragment(), rs.Some("fragment"))

	url, err = UrlParse("https://example.com/")
	itUnitOk(t, err)
	itUnitAssertEq(t, url.Fragment(), rs.None[string]())
}

func TestItUnit_test_set_ip_host(t *testing.T) {
	url, err := UrlParse("http://example.com")
	itUnitOk(t, err)

	itUnitOk(t, url.SetIpHost(netip.MustParseAddr("127.0.0.1")))
	itUnitAssertEq(t, url.HostStr(), rs.Some("127.0.0.1"))

	itUnitOk(t, url.SetIpHost(netip.MustParseAddr("::1")))
	itUnitAssertEq(t, url.HostStr(), rs.Some("[::1]"))
}

func TestItUnit_test_set_href(t *testing.T) {
	url, err := UrlParse("https://existing.url")
	itUnitOk(t, err)

	itUnitAssert(t, QuirksSetHref(&url, "mal//formed") != nil)

	itUnitAssert(t, QuirksSetHref(
		&url,
		"https://user:pass@domain.com:9742/path/file.ext?key=val&key2=val2#fragment",
	) == nil)
	expected, err := UrlParse("https://user:pass@domain.com:9742/path/file.ext?key=val&key2=val2#fragment")
	itUnitOk(t, err)
	itUnitAssert(t, url.Equal(&expected))
}

func TestItUnit_test_domain_encoding_quirks(t *testing.T) {
	data := [][3]string{
		{"http://example.com", "", ""},
		{"😅.🙂", "xn--j28h.xn--938h", "😅.🙂"},
		{"example.com", "example.com", "example.com"},
		{"mailto:test@example.net", "", ""},
	}

	for _, url := range data {
		itUnitAssertEq(t, QuirksDomainToAscii(url[0]), url[1])
		itUnitAssertEq(t, QuirksDomainToUnicode(url[0]), url[2])
	}
}

// test_expose_internals is compiled only with the crate's
// `expose_internals` feature, which `cargo test` does not turn on, and
// quirks::internal_components is not covered.

func TestItUnit_test_windows_unc_path(t *testing.T) {
	url, err := urlFromFilePath(`\\host\share\path\file.txt`, true)
	itUnitOk(t, err)
	itUnitAssertEq(t, url.AsStr(), "file://host/share/path/file.txt")

	url, err = urlFromFilePath(`\\höst\share\path\file.txt`, true)
	itUnitOk(t, err)
	itUnitAssertEq(t, url.AsStr(), "file://xn--hst-sna/share/path/file.txt")

	url, err = urlFromFilePath(`\\192.168.0.1\share\path\file.txt`, true)
	itUnitOk(t, err)
	itUnitAssertHost(t, url.Host(), &Host_Ipv4{F0: netip.AddrFrom4([4]byte{192, 168, 0, 1})})

	path, err := url.toFilePath(true)
	itUnitOk(t, err)
	itUnitAssertEq(t, path, `\\192.168.0.1\share\path\file.txt`)

	// Another way to write these:
	url, err = urlFromFilePath(`\\?\UNC\host\share\path\file.txt`, true)
	itUnitOk(t, err)
	itUnitAssertEq(t, url.AsStr(), "file://host/share/path/file.txt")

	// Paths starting with "\\.\" (Local Device Paths) are intentionally not supported.
	_, err = urlFromFilePath(`\\.\some\path\file.txt`, true)
	itUnitAssert(t, err != nil)
}

func TestItUnit_test_syntax_violation_callback(t *testing.T) {
	violation := rs.None[SyntaxViolation]()
	url, err := UrlOptions().
		SyntaxViolationCallback(rs.Some(func(v SyntaxViolation) { violation = rs.Some(v) })).
		Parse("http:////mozilla.org:42")
	itUnitOk(t, err)
	itUnitAssertEq(t, url.Port(), rs.Some[uint16](42))

	v := violation.Take().Unwrap()
	itUnitAssertEq(t, v, SyntaxViolation_ExpectedDoubleSlash)
	itUnitAssertEq(t, v.Description(), "expected //")
	itUnitAssertEq(t, v.String(), "expected //")
}

func TestItUnit_test_syntax_violation_callback_lifetimes(t *testing.T) {
	violation := rs.None[SyntaxViolation]()
	vfn := func(s SyntaxViolation) { violation = rs.Some(s) }

	url, err := UrlOptions().
		SyntaxViolationCallback(rs.Some(vfn)).
		Parse("http:////mozilla.org:42")
	itUnitOk(t, err)
	itUnitAssertEq(t, url.Port(), rs.Some[uint16](42))
	itUnitAssertEq(t, violation.Take(), rs.Some(SyntaxViolation_ExpectedDoubleSlash))

	url, err = UrlOptions().
		SyntaxViolationCallback(rs.Some(vfn)).
		Parse("http://mozilla.org\\path")
	itUnitOk(t, err)
	itUnitAssertEq(t, url.Path(), "/path")
	itUnitAssertEq(t, violation.Take(), rs.Some(SyntaxViolation_Backslash))
}

func TestItUnit_test_syntax_violation_callback_types(t *testing.T) {
	data := []struct {
		input       string
		violation   SyntaxViolation
		description string
	}{
		{"http://mozilla.org/\\foo", SyntaxViolation_Backslash, "backslash"},
		{" http://mozilla.org", SyntaxViolation_C0SpaceIgnored, "leading or trailing control or space character are ignored in URLs"},
		{"http://user:pass@mozilla.org", SyntaxViolation_EmbeddedCredentials, "embedding authentication information (username or password) in an URL is not recommended"},
		{"http:///mozilla.org", SyntaxViolation_ExpectedDoubleSlash, "expected //"},
		{"file:/foo.txt", SyntaxViolation_ExpectedFileDoubleSlash, "expected // after file:"},
		{"file://mozilla.org/c:/file.txt", SyntaxViolation_FileWithHostAndWindowsDrive, "file: with host and Windows drive letter"},
		{"http://mozilla.org/^", SyntaxViolation_NonUrlCodePoint, "non-URL code point"},
		{"http://mozilla.org/#\x000", SyntaxViolation_NullInFragment, "NULL characters are ignored in URL fragment identifiers"},
		{"http://mozilla.org/%1", SyntaxViolation_PercentDecode, "expected 2 hex digits after %"},
		{"http://mozilla.org\t/foo", SyntaxViolation_TabOrNewlineIgnored, "tabs or newlines are ignored in URLs"},
		{"http://user@:pass@mozilla.org", SyntaxViolation_UnencodedAtSign, "unencoded @ sign in username or password"},
	}

	for _, testCase := range data {
		violation := rs.None[SyntaxViolation]()
		_, err := UrlOptions().
			SyntaxViolationCallback(rs.Some(func(v SyntaxViolation) { violation = rs.Some(v) })).
			Parse(testCase.input)
		itUnitOk(t, err)

		v := violation.Take()
		itUnitAssertEq(t, v, rs.Some(testCase.violation))
		itUnitAssertEq(t, v.Unwrap().Description(), testCase.description)
		itUnitAssertEq(t, v.Unwrap().String(), testCase.description)
	}
}

func TestItUnit_test_options_reuse(t *testing.T) {
	var violations []SyntaxViolation
	vfn := func(v SyntaxViolation) { violations = append(violations, v) }

	options := UrlOptions().SyntaxViolationCallback(rs.Some(vfn))
	url, err := options.Parse("http:////mozilla.org")
	itUnitOk(t, err)

	options = options.BaseUrl(rs.Some(&url))
	url, err = options.Parse("/sub\\path")
	itUnitOk(t, err)
	itUnitAssertEq(t, url.AsStr(), "http://mozilla.org/sub/path")
	itUnitAssertEq(t, len(violations), 2)
	itUnitAssertEq(t, violations[0], SyntaxViolation_ExpectedDoubleSlash)
	itUnitAssertEq(t, violations[1], SyntaxViolation_Backslash)
}

// https://github.com/servo/rust-url/issues/505
//
// Rust has two tests of this name, one compiled for Windows and one for
// Unix; both run here.
func TestItUnit_test_url_from_file_path(t *testing.T) {
	// #[cfg(all(feature = "std", windows))]
	p := "c:///"
	u, err := urlFromFilePath(p, true)
	itUnitOk(t, err)
	path, err := u.toFilePath(true)
	itUnitOk(t, err)
	itUnitAssertEq(t, "C:\\", path)

	// #[cfg(any(unix, target_os = "redox", target_os = "wasi"))]
	p = "/c:/"
	u, err = urlFromFilePath(p, false)
	itUnitOk(t, err)
	path, err = u.toFilePath(false)
	itUnitOk(t, err)
	itUnitAssertEq(t, "/c:/", path)
}

func TestItUnit_test_non_special_path(t *testing.T) {
	dbUrl, err := UrlParse("postgres://postgres@localhost/")
	itUnitOk(t, err)
	itUnitAssertEq(t, dbUrl.AsStr(), "postgres://postgres@localhost/")
	dbUrl.SetPath("diesel_foo")
	itUnitAssertEq(t, dbUrl.AsStr(), "postgres://postgres@localhost/diesel_foo")
	itUnitAssertEq(t, dbUrl.Path(), "/diesel_foo")
}

func TestItUnit_test_non_special_path2(t *testing.T) {
	dbUrl, err := UrlParse("postgres://postgres@localhost/")
	itUnitOk(t, err)
	itUnitAssertEq(t, dbUrl.AsStr(), "postgres://postgres@localhost/")
	dbUrl.SetPath("")
	itUnitAssertEq(t, dbUrl.Path(), "")
	itUnitAssertEq(t, dbUrl.AsStr(), "postgres://postgres@localhost")
	dbUrl.SetPath("foo")
	itUnitAssertEq(t, dbUrl.Path(), "/foo")
	itUnitAssertEq(t, dbUrl.AsStr(), "postgres://postgres@localhost/foo")
	dbUrl.SetPath("/bar")
	itUnitAssertEq(t, dbUrl.Path(), "/bar")
	itUnitAssertEq(t, dbUrl.AsStr(), "postgres://postgres@localhost/bar")
}

func TestItUnit_test_non_special_path3(t *testing.T) {
	dbUrl, err := UrlParse("postgres://postgres@localhost/")
	itUnitOk(t, err)
	itUnitAssertEq(t, dbUrl.AsStr(), "postgres://postgres@localhost/")
	dbUrl.SetPath("/")
	itUnitAssertEq(t, dbUrl.AsStr(), "postgres://postgres@localhost/")
	itUnitAssertEq(t, dbUrl.Path(), "/")
	dbUrl.SetPath("/foo")
	itUnitAssertEq(t, dbUrl.AsStr(), "postgres://postgres@localhost/foo")
	itUnitAssertEq(t, dbUrl.Path(), "/foo")
}

func TestItUnit_test_set_scheme_to_file_with_host(t *testing.T) {
	url := itUnitParse(t, "http://localhost:6767/foo/bar")
	result := url.SetScheme("file")
	itUnitAssertEq(t, url.String(), "http://localhost:6767/foo/bar")
	itUnitAssertEq(t, result, errUnit)
}

func TestItUnit_test_set_scheme_empty_err(t *testing.T) {
	url := itUnitParse(t, "http://localhost:6767/foo/bar")
	result := url.SetScheme("")
	itUnitAssertEq(t, url.String(), "http://localhost:6767/foo/bar")
	itUnitAssertEq(t, result, errUnit)
}

func TestItUnit_no_panic(t *testing.T) {
	url, err := UrlParse("arhttpsps:/.//eom/dae.com/\\\\t\\:")
	itUnitOk(t, err)
	itUnitOk(t, QuirksSetHostname(&url, "//eom/datcom/\\\\t\\://eom/data.cs"))
}

func TestItUnit_test_null_host_with_leading_empty_path_segment(t *testing.T) {
	// since Note in item 3 of URL serializing in the URL Standard
	// https://url.spec.whatwg.org/#url-serializing
	url, err := UrlParse("m:/.//\\")
	itUnitOk(t, err)
	encoded := url.AsStr()
	reparsed, err := UrlParse(encoded)
	itUnitOk(t, err)
	itUnitAssert(t, reparsed.Equal(&url))
}

func TestItUnit_pop_if_empty_in_bounds(t *testing.T) {
	url, err := UrlParse("m://")
	itUnitOk(t, err)
	segments, err := url.PathSegmentsMut()
	itUnitOk(t, err)
	defer segments.Drop()
	segments.PopIfEmpty()
	segments.Pop()
}

func TestItUnit_test_slicing(t *testing.T) {
	type expectedSlices struct {
		full     string
		scheme   string
		username string
		password string
		host     string
		port     string
		path     string
		query    string
		fragment string
	}

	data := []expectedSlices{
		{
			full:     "https://user:pass@domain.com:9742/path/file.ext?key=val&key2=val2#fragment",
			scheme:   "https",
			username: "user",
			password: "pass",
			host:     "domain.com",
			port:     "9742",
			path:     "/path/file.ext",
			query:    "key=val&key2=val2",
			fragment: "fragment",
		},
		{
			full:     "https://domain.com:9742/path/file.ext#fragment",
			scheme:   "https",
			username: "",
			password: "",
			host:     "domain.com",
			port:     "9742",
			path:     "/path/file.ext",
			query:    "",
			fragment: "fragment",
		},
		{
			full:     "https://domain.com:9742/path/file.ext",
			scheme:   "https",
			username: "",
			password: "",
			host:     "domain.com",
			port:     "9742",
			path:     "/path/file.ext",
			query:    "",
			fragment: "",
		},
		{
			full:     "blob:blob-info",
			scheme:   "blob",
			username: "",
			password: "",
			host:     "",
			port:     "",
			path:     "blob-info",
			query:    "",
			fragment: "",
		},
	}

	for _, expected := range data {
		url, err := UrlParse(expected.full)
		itUnitOk(t, err)
		itUnitAssertEq(t, url.IndexRangeFull(), expected.full)
		itUnitAssertEq(t, url.IndexRange(Position_BeforeScheme, Position_AfterScheme), expected.scheme)
		itUnitAssertEq(t,
			url.IndexRange(Position_BeforeUsername, Position_AfterUsername),
			expected.username,
		)
		itUnitAssertEq(t,
			url.IndexRange(Position_BeforePassword, Position_AfterPassword),
			expected.password,
		)
		itUnitAssertEq(t, url.IndexRange(Position_BeforeHost, Position_AfterHost), expected.host)
		itUnitAssertEq(t, url.IndexRange(Position_BeforePort, Position_AfterPort), expected.port)
		itUnitAssertEq(t, url.IndexRange(Position_BeforePath, Position_AfterPath), expected.path)
		itUnitAssertEq(t, url.IndexRange(Position_BeforeQuery, Position_AfterQuery), expected.query)
		itUnitAssertEq(t,
			url.IndexRange(Position_BeforeFragment, Position_AfterFragment),
			expected.fragment,
		)
		itUnitAssertEq(t, url.IndexRangeTo(Position_AfterFragment), expected.full)
	}
}

func TestItUnit_test_make_relative(t *testing.T) {
	tests := [][3]string{
		{
			"http://127.0.0.1:8080/test",
			"http://127.0.0.1:8080/test",
			"",
		},
		{
			"http://127.0.0.1:8080/test",
			"http://127.0.0.1:8080/test/",
			"test/",
		},
		{
			"http://127.0.0.1:8080/test/",
			"http://127.0.0.1:8080/test",
			"../test",
		},
		{
			"http://127.0.0.1:8080/",
			"http://127.0.0.1:8080/?foo=bar#123",
			"?foo=bar#123",
		},
		{
			"http://127.0.0.1:8080/",
			"http://127.0.0.1:8080/test/video",
			"test/video",
		},
		{
			"http://127.0.0.1:8080/test",
			"http://127.0.0.1:8080/test/video",
			"test/video",
		},
		{
			"http://127.0.0.1:8080/test/",
			"http://127.0.0.1:8080/test/video",
			"video",
		},
		{
			"http://127.0.0.1:8080/test",
			"http://127.0.0.1:8080/test2/video",
			"test2/video",
		},
		{
			"http://127.0.0.1:8080/test/",
			"http://127.0.0.1:8080/test2/video",
			"../test2/video",
		},
		{
			"http://127.0.0.1:8080/test/bla",
			"http://127.0.0.1:8080/test2/video",
			"../test2/video",
		},
		{
			"http://127.0.0.1:8080/test/bla/",
			"http://127.0.0.1:8080/test2/video",
			"../../test2/video",
		},
		{
			"http://127.0.0.1:8080/test/?foo=bar#123",
			"http://127.0.0.1:8080/test/video",
			"video",
		},
		{
			"http://127.0.0.1:8080/test/",
			"http://127.0.0.1:8080/test/video?baz=meh#456",
			"video?baz=meh#456",
		},
		{
			"http://127.0.0.1:8080/test",
			"http://127.0.0.1:8080/test?baz=meh#456",
			"?baz=meh#456",
		},
		{
			"http://127.0.0.1:8080/test/",
			"http://127.0.0.1:8080/test?baz=meh#456",
			"../test?baz=meh#456",
		},
		{
			"http://127.0.0.1:8080/test/",
			"http://127.0.0.1:8080/test/?baz=meh#456",
			"?baz=meh#456",
		},
		{
			"http://127.0.0.1:8080/test/?foo=bar#123",
			"http://127.0.0.1:8080/test/video?baz=meh#456",
			"video?baz=meh#456",
		},
		{
			"http://127.0.0.1:8080/file.txt",
			"http://127.0.0.1:8080/test/file.txt",
			"test/file.txt",
		},
		{
			"http://127.0.0.1:8080/not_equal.txt",
			"http://127.0.0.1:8080/test/file.txt",
			"test/file.txt",
		},
	}

	for _, test := range tests {
		base, uri, relative := test[0], test[1], test[2]
		baseUri, err := UrlParse(base)
		itUnitOk(t, err)
		relativeUri, err := UrlParse(uri)
		itUnitOk(t, err)
		makeRelative := baseUri.MakeRelative(&relativeUri).Unwrap()
		itUnitAssertEq(t,
			makeRelative, relative,
			"base: {base}, uri: {uri}, relative: {relative}",
			rs.Arg("base", base), rs.Arg("uri", uri), rs.Arg("relative", relative),
		)
		joined, err := baseUri.Join(relative)
		itUnitOk(t, err)
		itUnitAssertEq(t,
			joined.AsStr(),
			uri,
			"base: {base}, uri: {uri}, relative: {relative}",
			rs.Arg("base", base), rs.Arg("uri", uri), rs.Arg("relative", relative),
		)
	}

	errorTests := [][2]string{
		{"http://127.0.0.1:8080/", "https://127.0.0.1:8080/test/"},
		{"http://127.0.0.1:8080/", "http://127.0.0.1:8081/test/"},
		{"http://127.0.0.1:8080/", "http://127.0.0.2:8080/test/"},
		{"mailto:a@example.com", "mailto:b@example.com"},
	}

	for _, test := range errorTests {
		base, uri := test[0], test[1]
		baseUri, err := UrlParse(base)
		itUnitOk(t, err)
		relativeUri, err := UrlParse(uri)
		itUnitOk(t, err)
		makeRelative := baseUri.MakeRelative(&relativeUri)
		itUnitAssertEq(t, makeRelative, rs.None[string](), "base: {base}, uri: {uri}", rs.Arg("base", base), rs.Arg("uri", uri))
	}
}

func TestItUnit_test_has_authority(t *testing.T) {
	url, err := UrlParse("mailto:joe@example.com")
	itUnitOk(t, err)
	itUnitAssert(t, !url.HasAuthority())
	url, err = UrlParse("unix:/run/foo.socket")
	itUnitOk(t, err)
	itUnitAssert(t, !url.HasAuthority())
	url, err = UrlParse("file:///tmp/foo")
	itUnitOk(t, err)
	itUnitAssert(t, url.HasAuthority())
	url, err = UrlParse("http://example.com/tmp/foo")
	itUnitOk(t, err)
	itUnitAssert(t, url.HasAuthority())
}

func TestItUnit_test_authority(t *testing.T) {
	cases := [][2]string{
		{"mailto:joe@example.com", ""},
		{"unix:/run/foo.socket", ""},
		{"file:///tmp/foo", ""},
		{"http://example.com/tmp/foo", "example.com"},
		{"ftp://127.0.0.1:21/", "127.0.0.1"},
		{"ftp://user@127.0.0.1:2121/", "user@127.0.0.1:2121"},
		{"https://:@example.com/", "example.com"},
		{"https://:password@[::1]:8080/", ":password@[::1]:8080"},
		{"gopher://user:@àlex.example.com:70", "user@%C3%A0lex.example.com:70"},
		{
			"irc://àlex:àlex@àlex.рф.example.com:6667/foo",
			"%C3%A0lex:%C3%A0lex@%C3%A0lex.%D1%80%D1%84.example.com:6667",
		},
		{
			"https://àlex:àlex@àlex.рф.example.com:443/foo",
			"%C3%A0lex:%C3%A0lex@xn--lex-8ka.xn--p1ai.example.com",
		},
	}
	for _, c := range cases {
		url, err := UrlParse(c[0])
		itUnitOk(t, err)
		itUnitAssertEq(t, url.Authority(), c[1])
	}
}

// https://github.com/servo/rust-url/issues/838
func TestItUnit_test_file_with_drive(t *testing.T) {
	s1 := "fIlE:p:?../"
	url, err := UrlParse(s1)
	itUnitOk(t, err)
	itUnitAssertEq(t, url.String(), "file:///p:?../")
	itUnitAssertEq(t, url.Path(), "/p:")

	testcases := [][2]string{
		{"a", "file:///p:/a"},
		{"", "file:///p:?../"},
		{"?x", "file:///p:?x"},
		{".", "file:///p:/"},
		{"..", "file:///p:/"},
		{"../", "file:///p:/"},
	}

	for _, c := range testcases {
		url2, err := url.Join(c[0])
		itUnitOk(t, err)
		itUnitAssertEq(t, url2.String(), c[1])
	}
}

// Similar to test_file_with_drive, but with a path
// that could be confused for a drive.
func TestItUnit_test_file_with_drive_and_path(t *testing.T) {
	s1 := "fIlE:p:/x|?../"
	url, err := UrlParse(s1)
	itUnitOk(t, err)
	itUnitAssertEq(t, url.String(), "file:///p:/x|?../")
	itUnitAssertEq(t, url.Path(), "/p:/x|")
	s2 := "a"
	url2, err := url.Join(s2)
	itUnitOk(t, err)
	itUnitAssertEq(t, url2.String(), "file:///p:/a")
}

func TestItUnit_issue_864(t *testing.T) {
	url, err := UrlParse("file://")
	itUnitOk(t, err)
	t.Log(rs.Format("[{}:{}:{}] &url = {:#?}", "tests/unit.rs", 1324, 5, &url))
	url.SetPath("x")
	t.Log(rs.Format("[{}:{}:{}] &url = {:#?}", "tests/unit.rs", 1326, 5, &url))
}

func TestItUnit_issue_974(t *testing.T) {
	url, err := UrlParse("http://example.com:8000")
	itUnitOk(t, err)
	_ = QuirksSetPort(&url, "\u00009000")
	itUnitAssertEq(t, url.Port(), rs.Some[uint16](8000))
}

// serde_error_message checks the message a serde_json deserialization of a
// struct with a Url field reports. go/crates/serde cannot hold a Url field
// yet (UrlDeserializeStr is what its Deserialize impl will call), so this
// checks the part of the message the url crate writes; serde_json appends
// " at line 1 column 25".
func TestItUnit_serde_error_message(t *testing.T) {
	_, err := UrlDeserializeStr("§invalid#+#*Ä")
	itUnitAssert(t, err != nil)
	itUnitAssertEq(t,
		err.Error(),
		`relative URL without a base: "§invalid#+#*Ä"`,
	)
}

func TestItUnit_test_parse_url_with_single_byte_control_host(t *testing.T) {
	input := "l://\x01:"

	url1, err := UrlParse(input)
	itUnitOk(t, err)
	url2, err := UrlParse(url1.AsStr())
	itUnitOk(t, err)
	itUnitAssert(t, url2.Equal(&url1))
}
