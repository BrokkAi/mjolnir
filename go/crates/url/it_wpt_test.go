// Port of url 2.5.8 tests/wpt.rs

package url_test

import (
	"encoding/json"
	"os"
	"path/filepath"
	"slices"
	"sort"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/url"
	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/strs"
)

// Data-driven tests imported from web-platform-tests
//
// The three files are the crate's tests/urltestdata.json,
// tests/setters_tests.json and tests/expected_failures.txt, copied unchanged
// into testdata/wpt. The Rust test is a `harness = false` binary whose
// `main` runs every case; this is that `main`. A filter (the Rust binary's
// first argument) is the environment variable URL_WPT_FILTER here.

type itWptUrlTest struct {
	input  string
	base   rs.Option[string]
	result itWptUrlTestResult
}

// itWptUrlTestResult is the `#[serde(untagged)]` enum UrlTestResult: Ok when
// the object has every member of UrlTestOk, Fail otherwise.
type itWptUrlTestResult interface{ isItWptUrlTestResult() }

type itWptUrlTestResult_Ok struct{ F0 itWptUrlTestOk }
type itWptUrlTestResult_Fail struct{ F0 itWptUrlTestFail }

func (*itWptUrlTestResult_Ok) isItWptUrlTestResult()   {}
func (*itWptUrlTestResult_Fail) isItWptUrlTestResult() {}

type itWptUrlTestOk struct {
	href     string
	protocol string
	username string
	password string
	host     string
	hostname string
	port     string
	pathname string
	search   string
	hash     string
}

type itWptUrlTestFail struct {
	failure bool
}

type itWptSetterTest struct {
	href     string
	newValue string
	expected itWptSetterTestExpected
}

type itWptSetterTestExpected struct {
	href     rs.Option[string]
	protocol rs.Option[string]
	username rs.Option[string]
	password rs.Option[string]
	host     rs.Option[string]
	hostname rs.Option[string]
	port     rs.Option[string]
	pathname rs.Option[string]
	search   rs.Option[string]
	hash     rs.Option[string]
}

func itWptReadFile(t *testing.T, name string) []byte {
	t.Helper()
	data, err := os.ReadFile(filepath.Join("testdata", "wpt", name))
	if err != nil {
		t.Fatal(err)
	}
	return data
}

// itWptString decodes the string member key, which the derived Deserialize
// requires.
func itWptString(t *testing.T, object map[string]json.RawMessage, key string) (string, bool) {
	t.Helper()
	raw, ok := object[key]
	if !ok {
		return "", false
	}
	var value string
	if err := json.Unmarshal(raw, &value); err != nil {
		return "", false
	}
	return value, true
}

// itWptOptionString decodes an `Option<String>` member: absent or null is
// None.
func itWptOptionString(t *testing.T, object map[string]json.RawMessage, key string) rs.Option[string] {
	t.Helper()
	raw, ok := object[key]
	if !ok || string(raw) == "null" {
		return rs.None[string]()
	}
	var value string
	if err := json.Unmarshal(raw, &value); err != nil {
		t.Fatalf("parsing failed: %s: %v", key, err)
	}
	return rs.Some(value)
}

func itWptDecodeUrlTest(t *testing.T, raw json.RawMessage) itWptUrlTest {
	t.Helper()
	var object map[string]json.RawMessage
	if err := json.Unmarshal(raw, &object); err != nil {
		t.Fatalf("parsing failed: %v", err)
	}
	input, ok := itWptString(t, object, "input")
	if !ok {
		t.Fatalf("parsing failed: no input in %s", raw)
	}
	test := itWptUrlTest{input: input, base: itWptOptionString(t, object, "base")}

	// The untagged enum tries UrlTestOk first.
	var ok2 itWptUrlTestOk
	fields := []*string{&ok2.href, &ok2.protocol, &ok2.username, &ok2.password, &ok2.host, &ok2.hostname, &ok2.port, &ok2.pathname, &ok2.search, &ok2.hash}
	names := []string{"href", "protocol", "username", "password", "host", "hostname", "port", "pathname", "search", "hash"}
	isOk := true
	for i, name := range names {
		value, present := itWptString(t, object, name)
		if !present {
			isOk = false
			break
		}
		*fields[i] = value
	}
	if isOk {
		test.result = &itWptUrlTestResult_Ok{F0: ok2}
		return test
	}
	var failure bool
	if err := json.Unmarshal(object["failure"], &failure); err != nil {
		t.Fatalf("parsing failed: data did not match any variant of untagged enum UrlTestResult: %s", raw)
	}
	test.result = &itWptUrlTestResult_Fail{F0: itWptUrlTestFail{failure: failure}}
	return test
}

func itWptDecodeSetterTest(t *testing.T, raw json.RawMessage) itWptSetterTest {
	t.Helper()
	var object map[string]json.RawMessage
	if err := json.Unmarshal(raw, &object); err != nil {
		t.Fatalf("parsing failed: %v", err)
	}
	href, ok := itWptString(t, object, "href")
	if !ok {
		t.Fatalf("parsing failed: no href in %s", raw)
	}
	newValue, ok := itWptString(t, object, "new_value")
	if !ok {
		t.Fatalf("parsing failed: no new_value in %s", raw)
	}
	var expected map[string]json.RawMessage
	if err := json.Unmarshal(object["expected"], &expected); err != nil {
		t.Fatalf("parsing failed: expected: %v", err)
	}
	return itWptSetterTest{
		href:     href,
		newValue: newValue,
		expected: itWptSetterTestExpected{
			href:     itWptOptionString(t, expected, "href"),
			protocol: itWptOptionString(t, expected, "protocol"),
			username: itWptOptionString(t, expected, "username"),
			password: itWptOptionString(t, expected, "password"),
			host:     itWptOptionString(t, expected, "host"),
			hostname: itWptOptionString(t, expected, "hostname"),
			port:     itWptOptionString(t, expected, "port"),
			pathname: itWptOptionString(t, expected, "pathname"),
			search:   itWptOptionString(t, expected, "search"),
			hash:     itWptOptionString(t, expected, "hash"),
		},
	}
}

type itWptError struct {
	name string
	err  string
}

func TestItWpt_main(t *testing.T) {
	filter := rs.None[string]()
	if value, ok := os.LookupEnv("URL_WPT_FILTER"); ok {
		filter = rs.Some(value)
	}

	expectedFailures := strs.Lines(string(itWptReadFile(t, "expected_failures.txt")))

	var errors []itWptError

	// Copied from https://github.com/web-platform-tests/wpt/blob/master/url/
	var urlJson []json.RawMessage
	if err := json.Unmarshal(itWptReadFile(t, "urltestdata.json"), &urlJson); err != nil {
		t.Fatalf("JSON parse error in urltestdata.json: %v", err)
	}
	var urlTests []itWptUrlTest
	for _, value := range urlJson {
		if !strings.HasPrefix(strings.TrimSpace(string(value)), "{") {
			continue
		}
		urlTests = append(urlTests, itWptDecodeUrlTest(t, value))
	}

	var setterJson map[string]json.RawMessage
	if err := json.Unmarshal(itWptReadFile(t, "setters_tests.json"), &setterJson); err != nil {
		t.Fatalf("JSON parse error in setters_tests.json: %v", err)
	}
	// Rust walks a HashMap, in no particular order; the result does not
	// depend on it. Sorting keeps the log stable.
	kinds := make([]string, 0, len(setterJson))
	for kind := range setterJson {
		if kind != "comment" {
			kinds = append(kinds, kind)
		}
	}
	sort.Strings(kinds)
	setterTests := map[string][]itWptSetterTest{}
	for _, kind := range kinds {
		var list []json.RawMessage
		if err := json.Unmarshal(setterJson[kind], &list); err != nil {
			t.Fatalf("parsing failed: %v", err)
		}
		for _, raw := range list {
			setterTests[kind] = append(setterTests[kind], itWptDecodeSetterTest(t, raw))
		}
	}

	for _, urlTest := range urlTests {
		name := rs.Format("<{}>", strs.EscapeDefault(urlTest.input))
		if base, ok := urlTest.base.Get(); ok {
			name += rs.Format(" against <{}>", strs.EscapeDefault(base))
		}
		if itWptShouldSkip(name, filter) {
			continue
		}

		res := itWptRunUrlTest(urlTest)
		itWptReport(t, name, res, &errors, &expectedFailures)
	}

	for _, kind := range kinds {
		for _, test := range setterTests[kind] {
			name := rs.Format(
				"<{}> set {} to <{}>",
				strs.EscapeDefault(test.href),
				kind,
				strs.EscapeDefault(test.newValue),
			)
			if itWptShouldSkip(name, filter) {
				continue
			}

			res := itWptRunSetterTest(kind, test)
			itWptReport(t, name, res, &errors, &expectedFailures)
		}
	}

	if len(errors) > 0 {
		var message strings.Builder
		message.WriteString("errors:\n\n")
		for _, e := range errors {
			message.WriteString(rs.Format("  name: {}\n", e.name))
			message.WriteString(rs.Format("  err:  {}\n\n", e.err))
		}
		t.Fatal(message.String())
	}

	if len(expectedFailures) > 0 && filter.IsNone() {
		var message strings.Builder
		message.WriteString("tests were expected to fail but did not run:\n\n")
		for _, name := range expectedFailures {
			message.WriteString(rs.Format("  {}\n", name))
		}
		message.WriteString("\nif these tests were removed, update expected_failures.txt\n")
		t.Fatal(message.String())
	}
}

func itWptShouldSkip(name string, filter rs.Option[string]) bool {
	if filter, ok := filter.Get(); ok {
		return !strings.Contains(name, filter)
	}
	return false
}

func itWptReport(
	t *testing.T,
	name string,
	res rs.Option[string],
	errors *[]itWptError,
	expectedFailures *[]string,
) {
	t.Helper()
	expectedFailure := slices.Contains(*expectedFailures, name)
	*expectedFailures = slices.DeleteFunc(*expectedFailures, func(s string) bool { return s == name })
	if err, failed := res.Get(); failed {
		if expectedFailure {
			t.Logf("%s ... ✅ (expected fail)", name)
		} else {
			t.Logf("%s ... ❌", name)
			*errors = append(*errors, itWptError{name: name, err: err})
		}
		return
	}
	if expectedFailure {
		t.Logf("%s ... 🟠 (unexpected success)", name)
		*errors = append(*errors, itWptError{name: name, err: "unexpected success"})
	}
}

// itWptRunUrlTest is run_url_test. Its `Result<(), String>` is the error
// text, or None for Ok.
func itWptRunUrlTest(test itWptUrlTest) rs.Option[string] {
	base := rs.None[*url.Url]()
	if baseText, ok := test.base.Get(); ok {
		parsed, err := url.UrlParse(baseText)
		if err != nil {
			return rs.Some(rs.Format("errored while parsing base: {}", err))
		}
		base = rs.Some(&parsed)
	}

	res, err := url.UrlOptions().BaseUrl(base).Parse(test.input)
	errText := ""
	if err != nil {
		errText = rs.Format("errored while parsing input: {}", err)
	}

	switch result := test.result.(type) {
	case *itWptUrlTestResult_Ok:
		return itWptCheckUrlOk(res, err, errText, result.F0)
	case *itWptUrlTestResult_Fail:
		rs.Assert(result.F0.failure, "assertion failed: fail.failure")
		if err == nil {
			return rs.Some("expected failure, but parsed successfully")
		}

		return rs.None[string]()
	default:
		panic(rs.Unreachable(result))
	}
}

func itWptCheckUrlOk(res url.Url, err error, errText string, ok itWptUrlTestOk) rs.Option[string] {
	if err != nil {
		return rs.Some(rs.Format("expected success, but errored: {:?}", errText))
	}
	u := &res

	href := url.QuirksHref(u)
	if href != ok.href {
		return rs.Some(rs.Format("expected href {:?}, but got {:?}", ok.href, href))
	}

	protocol := url.QuirksProtocol(u)
	if protocol != ok.protocol {
		return rs.Some(rs.Format("expected protocol {:?}, but got {:?}", ok.protocol, protocol))
	}

	username := url.QuirksUsername(u)
	if username != ok.username {
		return rs.Some(rs.Format("expected username {:?}, but got {:?}", ok.username, username))
	}

	password := url.QuirksPassword(u)
	if password != ok.password {
		return rs.Some(rs.Format("expected password {:?}, but got {:?}", ok.password, password))
	}

	host := url.QuirksHost(u)
	if host != ok.host {
		return rs.Some(rs.Format("expected host {:?}, but got {:?}", ok.host, host))
	}

	hostname := url.QuirksHostname(u)
	if hostname != ok.hostname {
		return rs.Some(rs.Format("expected hostname {:?}, but got {:?}", ok.hostname, hostname))
	}

	port := url.QuirksPort(u)
	if port != ok.port {
		return rs.Some(rs.Format("expected port {:?}, but got {:?}", ok.port, port))
	}

	pathname := url.QuirksPathname(u)
	if pathname != ok.pathname {
		return rs.Some(rs.Format("expected pathname {:?}, but got {:?}", ok.pathname, pathname))
	}

	search := url.QuirksSearch(u)
	if search != ok.search {
		return rs.Some(rs.Format("expected search {:?}, but got {:?}", ok.search, search))
	}

	hash := url.QuirksHash(u)
	if hash != ok.hash {
		return rs.Some(rs.Format("expected hash {:?}, but got {:?}", ok.hash, hash))
	}

	return rs.None[string]()
}

func itWptRunSetterTest(kind string, test itWptSetterTest) rs.Option[string] {
	parsed, err := url.UrlParse(test.href)
	if err != nil {
		return rs.Some(rs.Format("errored while parsing href: {}", err))
	}
	u := &parsed
	newValue := test.newValue

	switch kind {
	case "protocol":
		_ = url.QuirksSetProtocol(u, newValue)
	case "username":
		_ = url.QuirksSetUsername(u, newValue)
	case "password":
		_ = url.QuirksSetPassword(u, newValue)
	case "host":
		_ = url.QuirksSetHost(u, newValue)
	case "hostname":
		_ = url.QuirksSetHostname(u, newValue)
	case "port":
		_ = url.QuirksSetPort(u, newValue)
	case "pathname":
		url.QuirksSetPathname(u, newValue)
	case "search":
		url.QuirksSetSearch(u, newValue)
	case "hash":
		url.QuirksSetHash(u, newValue)
	default:
		return rs.Some(rs.Format("unknown setter kind: {:?}", kind))
	}

	checks := []struct {
		name     string
		expected rs.Option[string]
		get      func(*url.Url) string
	}{
		{"href", test.expected.href, url.QuirksHref},
		{"protocol", test.expected.protocol, url.QuirksProtocol},
		{"username", test.expected.username, url.QuirksUsername},
		{"password", test.expected.password, url.QuirksPassword},
		{"host", test.expected.host, url.QuirksHost},
		{"hostname", test.expected.hostname, url.QuirksHostname},
		{"port", test.expected.port, url.QuirksPort},
		{"pathname", test.expected.pathname, url.QuirksPathname},
		{"search", test.expected.search, url.QuirksSearch},
		{"hash", test.expected.hash, url.QuirksHash},
	}
	for _, check := range checks {
		if expected, ok := check.expected.Get(); ok {
			got := check.get(u)
			if got != expected {
				return rs.Some("expected " + check.name + " " + rs.DebugString(expected) + ", but got " + rs.DebugString(got))
			}
		}
	}

	return rs.None[string]()
}
