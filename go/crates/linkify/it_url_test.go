// Port of linkify 0.11.0 tests/url.rs

package linkify_test

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/linkify"
)

func TestItUrl_no_links(t *testing.T) {
	itUrlAssertNotLinked(t, "")
	itUrlAssertNotLinked(t, "foo")
	itUrlAssertNotLinked(t, ":")
	itUrlAssertNotLinked(t, "://")
	itUrlAssertNotLinked(t, ":::")
}

func TestItUrl_schemes(t *testing.T) {
	itUrlAssertNotLinked(t, "://foo")
	itUrlAssertNotLinked(t, "1://foo")
	itUrlAssertNotLinked(t, "123://foo")
	itUrlAssertNotLinked(t, "+://foo")
	itUrlAssertNotLinked(t, "-://foo")
	itUrlAssertNotLinked(t, ".://foo")
	itUrlAssertNotLinked(t, "1abc://foo")
	itUrlAssertLinked(t, "a://foo", "|a://foo|")
	itUrlAssertLinked(t, "a123://foo", "|a123://foo|")
	itUrlAssertLinked(t, "a123b://foo", "|a123b://foo|")
	itUrlAssertLinked(t, "a+b://foo", "|a+b://foo|")
	itUrlAssertLinked(t, "a-b://foo", "|a-b://foo|")
	itUrlAssertLinked(t, "a.b://foo", "|a.b://foo|")
	itUrlAssertLinked(t, "ABC://foo", "|ABC://foo|")
	itUrlAssertLinked(t, ".http://example.org/", ".|http://example.org/|")
	itUrlAssertLinked(t, "1.http://example.org/", "1.|http://example.org/|")
}

func TestItUrl_authority(t *testing.T) {
	itUrlAssertNotLinked(t, "ab://")
	itUrlAssertNotLinked(t, "file://")
	itUrlAssertNotLinked(t, "file:// ")
	itUrlAssertNotLinked(t, "\"file://\"")
	itUrlAssertNotLinked(t, "\"file://...\", ")
	itUrlAssertLinked(t, "file://somefile", "|file://somefile|")
	itUrlAssertLinked(t, "file://../relative", "|file://../relative|")
	itUrlAssertLinked(t, "http://a.", "|http://a|.")
}

func TestItUrl_local_links(t *testing.T) {
	itUrlAssertLinked(t, "http://127.0.0.1", "|http://127.0.0.1|")
	itUrlAssertLinked(t, "http://127.0.0.1/", "|http://127.0.0.1/|")
}

func TestItUrl_single_links(t *testing.T) {
	itUrlAssertLinked(t, "ab://c", "|ab://c|")
	itUrlAssertLinked(t, "http://example.org/", "|http://example.org/|")
	itUrlAssertLinked(t, "http://example.org/123", "|http://example.org/123|")
	itUrlAssertLinked(t,
		"http://example.org/?foo=test&bar=123",
		"|http://example.org/?foo=test&bar=123|",
	)
	itUrlAssertLinked(t,
		"http://example.org/?foo=%20",
		"|http://example.org/?foo=%20|",
	)
	itUrlAssertLinked(t, "http://example.org/%3C", "|http://example.org/%3C|")
}

func TestItUrl_single_links_without_protocol(t *testing.T) {
	itUrlAssertUrlsWithoutProtocol(t, "example.org/", "|example.org/|")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/123", "|example.org/123|")
	itUrlAssertUrlsWithoutProtocol(t,
		"example.org/?foo=test&bar=123",
		"|example.org/?foo=test&bar=123|",
	)
	itUrlAssertUrlsWithoutProtocol(t, "example.org/?foo=%20", "|example.org/?foo=%20|")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/%3C", "|example.org/%3C|")
}

func TestItUrl_space_characters_stop_url(t *testing.T) {
	itUrlAssertLinked(t, "foo http://example.org/", "foo |http://example.org/|")
	itUrlAssertLinked(t, "http://example.org/ bar", "|http://example.org/| bar")
	itUrlAssertLinked(t, "http://example.org/\tbar", "|http://example.org/|\tbar")
	itUrlAssertLinked(t, "http://example.org/\nbar", "|http://example.org/|\nbar")
	itUrlAssertLinked(t,
		"http://example.org/\x0bbar",
		"|http://example.org/|\x0bbar",
	)
	itUrlAssertLinked(t,
		"http://example.org/\x0cbar",
		"|http://example.org/|\x0cbar",
	)
	itUrlAssertLinked(t, "http://example.org/\rbar", "|http://example.org/|\rbar")
}

func TestItUrl_space_characters_stop_url_without_protocol(t *testing.T) {
	itUrlAssertUrlsWithoutProtocol(t, "foo example.org/", "foo |example.org/|")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/ bar", "|example.org/| bar")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/\tbar", "|example.org/|\tbar")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/\nbar", "|example.org/|\nbar")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/\x0bbar", "|example.org/|\x0bbar")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/\x0cbar", "|example.org/|\x0cbar")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/\rbar", "|example.org/|\rbar")
}

func TestItUrl_illegal_characters_stop_url(t *testing.T) {
	itUrlAssertLinked(t, "http://example.org/<", "|http://example.org/|<")
	itUrlAssertLinked(t, "http://example.org/>", "|http://example.org/|>")
	itUrlAssertLinked(t, "http://example.org/<>", "|http://example.org/|<>")
	itUrlAssertLinked(t, "http://example.org/\x00", "|http://example.org/|\x00")
	itUrlAssertLinked(t, "http://example.org/\x0e", "|http://example.org/|\x0e")
	itUrlAssertLinked(t, "http://example.org/\x7f", "|http://example.org/|\x7f")
	itUrlAssertLinked(t, "http://example.org/\u009f", "|http://example.org/|\u009f")
	itUrlAssertLinked(t, "http://example.org/foo|bar", "|http://example.org/foo||bar")
}

func TestItUrl_illegal_characters_stop_url_without_protocol(t *testing.T) {
	itUrlAssertUrlsWithoutProtocol(t, "example.org/<", "|example.org/|<")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/>", "|example.org/|>")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/<>", "|example.org/|<>")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/\x00", "|example.org/|\x00")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/\x0e", "|example.org/|\x0e")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/\x7f", "|example.org/|\x7f")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/\u009f", "|example.org/|\u009f")
}

func TestItUrl_delimiter_at_end(t *testing.T) {
	itUrlAssertLinked(t, "http://example.org/.", "|http://example.org/|.")
	itUrlAssertLinked(t, "http://example.org/..", "|http://example.org/|..")
	itUrlAssertLinked(t, "http://example.org/,", "|http://example.org/|,")
	itUrlAssertLinked(t, "http://example.org/:", "|http://example.org/|:")
	itUrlAssertLinked(t, "http://example.org/?", "|http://example.org/|?")
	itUrlAssertLinked(t, "http://example.org/!", "|http://example.org/|!")
	itUrlAssertLinked(t, "http://example.org/;", "|http://example.org/|;")
}

func TestItUrl_delimiter_at_end_no_protocol(t *testing.T) {
	itUrlAssertUrlsWithoutProtocol(t, "example.org/.", "|example.org/|.")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/..", "|example.org/|..")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/,", "|example.org/|,")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/:", "|example.org/|:")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/?", "|example.org/|?")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/!", "|example.org/|!")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/;", "|example.org/|;")
}

func TestItUrl_delimiter_followed_by_slash(t *testing.T) {
	// https://github.com/robinst/linkify/issues/90
	itUrlAssertLinked(t, "http://example.org/!/", "|http://example.org/!/|")
	itUrlAssertLinked(t, "http://example.org/test!/", "|http://example.org/test!/|")
	itUrlAssertLinked(t, "http://example.org/./", "|http://example.org/./|")
	itUrlAssertLinked(t, "http://example.org/,/", "|http://example.org/,/|")
	itUrlAssertLinked(t, "http://example.org/:/", "|http://example.org/:/|")
	itUrlAssertLinked(t, "http://example.org/;/", "|http://example.org/;/|")
	itUrlAssertLinked(t, "http://example.org/?/", "|http://example.org/?/|")
	// Delimiter followed by slash and more path
	itUrlAssertLinked(t, "http://example.org/!/a", "|http://example.org/!/a|")
	itUrlAssertLinked(t, "http://example.org/test!/a", "|http://example.org/test!/a|")
}

func TestItUrl_delimiter_followed_by_slash_no_protocol(t *testing.T) {
	itUrlAssertUrlsWithoutProtocol(t, "example.org/!/", "|example.org/!/|")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/test!/", "|example.org/test!/|")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/./", "|example.org/./|")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/,/", "|example.org/,/|")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/;/", "|example.org/;/|")
}

func TestItUrl_matching_punctuation(t *testing.T) {
	itUrlAssertLinked(t, "http://example.org/a(b)", "|http://example.org/a(b)|")
	itUrlAssertLinked(t, "http://example.org/a[b]", "|http://example.org/a[b]|")
	itUrlAssertLinked(t, "http://example.org/a{b}", "|http://example.org/a{b}|")
	itUrlAssertLinked(t, "http://example.org/a'b'", "|http://example.org/a'b'|")
	itUrlAssertLinked(t, "(http://example.org/)", "(|http://example.org/|)")
	itUrlAssertLinked(t, "[http://example.org/]", "[|http://example.org/|]")
	itUrlAssertLinked(t, "{http://example.org/}", "{|http://example.org/|}")
	itUrlAssertLinked(t, "\"http://example.org/\"", "\"|http://example.org/|\"")
	itUrlAssertLinked(t, "'http://example.org/'", "'|http://example.org/|'")
}
func TestItUrl_matching_punctuation_without_protocol(t *testing.T) {
	itUrlAssertUrlsWithoutProtocol(t, "example.org/a(b)", "|example.org/a(b)|")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/a[b]", "|example.org/a[b]|")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/a{b}", "|example.org/a{b}|")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/a'b'", "|example.org/a'b'|")
	itUrlAssertUrlsWithoutProtocol(t, "(example.org/)", "(|example.org/|)")
	itUrlAssertUrlsWithoutProtocol(t, "[example.org/]", "[|example.org/|]")
	itUrlAssertUrlsWithoutProtocol(t, "{example.org/}", "{|example.org/|}")
	itUrlAssertUrlsWithoutProtocol(t, "\"example.org/\"", "\"|example.org/|\"")
	itUrlAssertUrlsWithoutProtocol(t, "'example.org/'", "'|example.org/|'")
}

func TestItUrl_matching_punctuation_tricky(t *testing.T) {
	itUrlAssertLinked(t, "((http://example.org/))", "((|http://example.org/|))")
	itUrlAssertLinked(t,
		"((http://example.org/a(b)))",
		"((|http://example.org/a(b)|))",
	)
	itUrlAssertLinked(t, "[(http://example.org/)]", "[(|http://example.org/|)]")
	itUrlAssertLinked(t, "(http://example.org/).", "(|http://example.org/|).")
	itUrlAssertLinked(t, "(http://example.org/.)", "(|http://example.org/|.)")
	itUrlAssertLinked(t, "http://example.org/>", "|http://example.org/|>")
	// not sure about these
	itUrlAssertLinked(t, "http://example.org/(", "|http://example.org/|(")
	itUrlAssertLinked(t, "http://example.org/(.", "|http://example.org/|(.")
	itUrlAssertLinked(t, "http://example.org/]()", "|http://example.org/|]()")
}

func TestItUrl_matching_punctuation_tricky_without_protocol(t *testing.T) {
	itUrlAssertUrlsWithoutProtocol(t, "((example.org/))", "((|example.org/|))")
	itUrlAssertUrlsWithoutProtocol(t, "((example.org/a(b)))", "((|example.org/a(b)|))")
	itUrlAssertUrlsWithoutProtocol(t, "[(example.org/)]", "[(|example.org/|)]")
	itUrlAssertUrlsWithoutProtocol(t, "(example.org/).", "(|example.org/|).")
	itUrlAssertUrlsWithoutProtocol(t, "(example.org/.)", "(|example.org/|.)")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/>", "|example.org/|>")
	// not sure about these
	itUrlAssertUrlsWithoutProtocol(t, "example.org/(", "|example.org/|(")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/(.", "|example.org/|(.")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/]()", "|example.org/|]()")
}

func TestItUrl_single_quote(t *testing.T) {
	itUrlAssertLinked(t, "'https://example.org'", "'|https://example.org|'")
	itUrlAssertLinked(t, "\"https://example.org\"", "\"|https://example.org|\"")
	itUrlAssertLinked(t, "''https://example.org''", "''|https://example.org|''")
	itUrlAssertLinked(t, "'https://example.org''", "'|https://example.org|''")
	itUrlAssertLinked(t, "'https://example.org", "'|https://example.org|")
	itUrlAssertLinked(t,
		"http://example.org/'_(foo)",
		"|http://example.org/'_(foo)|",
	)
	itUrlAssertLinked(t,
		"http://example.org/'_(foo)'",
		"|http://example.org/'_(foo)'|",
	)
	itUrlAssertLinked(t, "http://example.org/''", "|http://example.org/''|")
	itUrlAssertLinked(t, "http://example.org/'''", "|http://example.org/''|'")
	itUrlAssertLinked(t, "http://example.org/'.", "|http://example.org/|'.")
	itUrlAssertLinked(t, "http://example.org/'a", "|http://example.org/'a|")
	itUrlAssertLinked(t, "http://example.org/it's", "|http://example.org/it's|")
}

func TestItUrl_single_quote_without_protocol(t *testing.T) {
	itUrlAssertUrlsWithoutProtocol(t, "example.org/'_(foo)", "|example.org/'_(foo)|")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/'_(foo)'", "|example.org/'_(foo)'|")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/''", "|example.org/''|")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/'''", "|example.org/''|'")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/'.", "|example.org/|'.")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/'a", "|example.org/'a|")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/it's", "|example.org/it's|")
}

func TestItUrl_double_quote(t *testing.T) {
	// " not allowed in URLs
	itUrlAssertLinked(t, "http://example.org/\"a", "|http://example.org/|\"a")
	itUrlAssertLinked(t, "http://example.org/\"a\"", "|http://example.org/|\"a\"")
}

func TestItUrl_grave_quote(t *testing.T) {
	// ` not allowed in URLs
	itUrlAssertLinked(t, "http://example.org/`a", "|http://example.org/|`a")
	itUrlAssertLinked(t, "http://example.org/`a`", "|http://example.org/|`a`")
}

func TestItUrl_asterisk(t *testing.T) {
	itUrlAssertLinked(t, "https://example.org*", "|https://example.org|*")
	itUrlAssertLinked(t, "https://example.org/*", "|https://example.org/|*")
	itUrlAssertLinked(t, "https://example.org/**", "|https://example.org/|**")
	itUrlAssertLinked(t, "https://example.org/*/a", "|https://example.org/*/a|")
}

func TestItUrl_grave_quote_without_protocol(t *testing.T) {
	// ` not allowed in URLs
	itUrlAssertUrlsWithoutProtocol(t, "example.org/`a", "|example.org/|`a")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/`a`", "|example.org/|`a`")
}

func TestItUrl_html(t *testing.T) {
	itUrlAssertLinked(t, "http://example.org\">", "|http://example.org|\">")
	itUrlAssertLinked(t, "http://example.org'>", "|http://example.org|'>")
	itUrlAssertLinked(t, "http://example.org\"/>", "|http://example.org|\"/>")
	itUrlAssertLinked(t, "http://example.org'/>", "|http://example.org|'/>")
	itUrlAssertLinked(t, "http://example.org<p>", "|http://example.org|<p>")
	itUrlAssertLinked(t, "http://example.org</p>", "|http://example.org|</p>")
}

func TestItUrl_html_no_protocol(t *testing.T) {
	itUrlAssertUrlsWithoutProtocol(t, "example.org\">", "|example.org|\">")
	itUrlAssertUrlsWithoutProtocol(t, "example.org'>", "|example.org|'>")
	itUrlAssertUrlsWithoutProtocol(t, "example.org\"/>", "|example.org|\"/>")
	itUrlAssertUrlsWithoutProtocol(t, "example.org'/>", "|example.org|'/>")
	itUrlAssertUrlsWithoutProtocol(t, "example.org<p>", "|example.org|<p>")
	itUrlAssertUrlsWithoutProtocol(t, "example.org</p>", "|example.org|</p>")
}

func TestItUrl_css(t *testing.T) {
	itUrlAssertLinked(t, "http://example.org\");", "|http://example.org|\");")
	itUrlAssertLinked(t, "http://example.org');", "|http://example.org|');")
}

func TestItUrl_images(t *testing.T) {
	itUrlAssertLinked(t,
		"<img src=\"http://example.org/test.svg\">",
		"<img src=\"|http://example.org/test.svg|\">",
	)
}

func TestItUrl_complex_html(t *testing.T) {
	itUrlAssertLinked(t,
		"<div><a href=\"http://example.org\"></a></div>",
		"<div><a href=\"|http://example.org|\"></a></div>",
	)

	itUrlAssertLinked(t,
		"<div><a href=\"http://example.org\"\n        ></a></div>",
		"<div><a href=\"|http://example.org|\"\n        ></a></div>",
	)

	itUrlAssertLinked(t,
		"<div>\n       <img\n         src=\"http://example.org/test3.jpg\" />\n     </div>",
		"<div>\n       <img\n         src=\"|http://example.org/test3.jpg|\" />\n     </div>",
	)
}

func TestItUrl_css_without_protocol(t *testing.T) {
	itUrlAssertUrlsWithoutProtocol(t, "example.org\");", "|example.org|\");")
	itUrlAssertUrlsWithoutProtocol(t, "example.org');", "|example.org|');")
}

func TestItUrl_slash(t *testing.T) {
	itUrlAssertLinked(t, "http://example.org/", "|http://example.org/|")
	itUrlAssertLinked(t, "http://example.org/a/", "|http://example.org/a/|")
	itUrlAssertLinked(t, "http://example.org//", "|http://example.org//|")
}

func TestItUrl_slash_without_protocol(t *testing.T) {
	itUrlAssertUrlsWithoutProtocol(t, "example.org/", "|example.org/|")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/a/", "|example.org/a/|")
	itUrlAssertUrlsWithoutProtocol(t, "example.org//", "|example.org//|")
}

func TestItUrl_multiple(t *testing.T) {
	itUrlAssertLinked(t,
		"http://one.org/ http://two.org/",
		"|http://one.org/| |http://two.org/|",
	)
	itUrlAssertLinked(t,
		"http://one.org/ : http://two.org/",
		"|http://one.org/| : |http://two.org/|",
	)
	itUrlAssertLinked(t,
		"(http://one.org/)(http://two.org/)",
		"(|http://one.org/|)(|http://two.org/|)",
	)
}
func TestItUrl_multiple_without_protocol(t *testing.T) {
	itUrlAssertUrlsWithoutProtocol(t, "one.org/ two.org/", "|one.org/| |two.org/|")
	itUrlAssertUrlsWithoutProtocol(t, "one.org/ : two.org/", "|one.org/| : |two.org/|")
	itUrlAssertUrlsWithoutProtocol(t, "(one.org/)(two.org/)", "(|one.org/|)(|two.org/|)")
}

func TestItUrl_multiple_mixed_protocol(t *testing.T) {
	itUrlAssertUrlsWithoutProtocol(t, "http://one.org/ two.org/", "|http://one.org/| |two.org/|")
	itUrlAssertUrlsWithoutProtocol(t,
		"one.org/ : http://two.org/",
		"|one.org/| : |http://two.org/|",
	)
	itUrlAssertUrlsWithoutProtocol(t,
		"(http://one.org/)(two.org/)",
		"(|http://one.org/|)(|two.org/|)",
	)
}

func TestItUrl_international(t *testing.T) {
	itUrlAssertLinked(t, "http://üñîçøðé.com", "|http://üñîçøðé.com|")
	itUrlAssertLinked(t, "http://üñîçøðé.com/ä", "|http://üñîçøðé.com/ä|")
	itUrlAssertLinked(t, "http://example.org/\u00a1", "|http://example.org/\u00a1|")
	itUrlAssertLinked(t, "http://example.org/\u00a2", "|http://example.org/\u00a2|")
	itUrlAssertLinked(t,
		"http://example.org/\U0001f600",
		"|http://example.org/\U0001f600|",
	)
	itUrlAssertLinked(t, "http://example.org/\u00a2/", "|http://example.org/\u00a2/|")
	itUrlAssertLinked(t,
		"http://xn--c1h.example.com/",
		"|http://xn--c1h.example.com/|",
	)
}

func TestItUrl_international_not_allowed(t *testing.T) {
	finder := linkify.LinkFinderNew()
	finder.UrlCanBeIri(false)
	finder.UrlMustHaveScheme(true)
	finder.Kinds([]linkify.LinkKind{linkify.LinkKind_Url})
	itCommonAssertLinkedWith(t, finder, "http://üñîçøðé.com", "http://üñîçøðé.com")
	itCommonAssertLinkedWith(t, finder, "http://üñîçøðé.com/ä", "http://üñîçøðé.com/ä")
	itCommonAssertLinkedWith(t,
		finder,
		"http://example.org/\u00a1",
		"|http://example.org/|\u00a1",
	)
	itCommonAssertLinkedWith(t,
		finder,
		"http://example.org/\u00a2",
		"|http://example.org/|\u00a2",
	)
	itCommonAssertLinkedWith(t,
		finder,
		"http://example.org/\U0001f600",
		"|http://example.org/|\U0001f600",
	)
	itCommonAssertLinkedWith(t,
		finder,
		"http://example.org/\u00a2/",
		"|http://example.org/|\u00a2/",
	)
	itCommonAssertLinkedWith(t,
		finder,
		"http://xn--c1h.example.com/",
		"|http://xn--c1h.example.com/|",
	)
}

func TestItUrl_international_not_allowed_without_protocol(t *testing.T) {
	finder := linkify.LinkFinderNew()
	finder.UrlCanBeIri(false)
	finder.UrlMustHaveScheme(false)
	finder.Kinds([]linkify.LinkKind{linkify.LinkKind_Url})
	itCommonAssertLinkedWith(t, finder, "üñîçøðé.com", "üñîçøðé.com")
	itCommonAssertLinkedWith(t, finder, "üñîçøðé.com/ä", "üñîçøðé.com/ä")
	itCommonAssertLinkedWith(t, finder, "example.org/\u00a1", "|example.org/|\u00a1")
	itCommonAssertLinkedWith(t, finder, "example.org/\u00a2", "|example.org/|\u00a2")
	itCommonAssertLinkedWith(t, finder, "example.org/\U0001f600", "|example.org/|\U0001f600")
	itCommonAssertLinkedWith(t, finder, "example.org/\u00a2/", "|example.org/|\u00a2/")
	itCommonAssertLinkedWith(t, finder, "xn--c1h.example.com/", "|xn--c1h.example.com/|")
}

func TestItUrl_international_without_protocol(t *testing.T) {
	itUrlAssertUrlsWithoutProtocol(t, "üñîçøðé.com", "|üñîçøðé.com|")
	itUrlAssertUrlsWithoutProtocol(t, "üñîçøðé.com/ä", "|üñîçøðé.com/ä|")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/\u00a1", "|example.org/\u00a1|")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/\u00a2", "|example.org/\u00a2|")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/\U0001f600", "|example.org/\U0001f600|")
	itUrlAssertUrlsWithoutProtocol(t, "example.org/\u00a2/", "|example.org/\u00a2/|")
	itUrlAssertUrlsWithoutProtocol(t, "xn--c1h.example.com/", "|xn--c1h.example.com/|")
}

func TestItUrl_domain_tld_without_protocol_must_be_long(t *testing.T) {
	itUrlAssertUrlsWithoutProtocol(t, "example.", "example.")
	itUrlAssertUrlsWithoutProtocol(t, "example./", "example./")
	itUrlAssertUrlsWithoutProtocol(t, "foo.com.", "|foo.com|.")
	itUrlAssertUrlsWithoutProtocol(t, "example.c", "example.c")
	itUrlAssertUrlsWithoutProtocol(t, "example.co", "|example.co|")
	itUrlAssertUrlsWithoutProtocol(t, "example.com", "|example.com|")
	itUrlAssertUrlsWithoutProtocol(t, "e.com", "|e.com|")
	itUrlAssertUrlsWithoutProtocol(t, "exampl.e.c", "exampl.e.c")
	itUrlAssertUrlsWithoutProtocol(t, "exampl.e.co", "|exampl.e.co|")
	itUrlAssertUrlsWithoutProtocol(t, "e.xample.c", "e.xample.c")
	itUrlAssertUrlsWithoutProtocol(t, "e.xample.co", "|e.xample.co|")
	itUrlAssertUrlsWithoutProtocol(t, "v1.1.1", "v1.1.1")
}

func TestItUrl_skip_emails_without_protocol(t *testing.T) {
	itUrlAssertNotLinkedWithoutProtocol(t, "foo.bar@example.org")
	itUrlAssertNotLinkedWithoutProtocol(t, "example.com@example.com")
}

func TestItUrl_avoid_multiple_matches_without_protocol(t *testing.T) {
	finder := linkify.LinkFinderNew()
	finder.UrlMustHaveScheme(false)
	links := finder.Links("http://example.com").Collect()
	itCommonAssertEq(t, len(links), 1)
	itCommonAssertEq(t, links[0].AsStr(), "http://example.com")
}

func TestItUrl_without_protocol_and_email(t *testing.T) {
	finder := linkify.LinkFinderNew()
	finder.UrlMustHaveScheme(false)

	itCommonAssertLinkedWith(t,
		finder,
		"Look, no scheme: example.org/foo email@foo.com",
		"Look, no scheme: |example.org/foo| |email@foo.com|",
	)

	itCommonAssertLinkedWith(t,
		finder,
		"Web:\nwww.foobar.co\nE-Mail:\n      bar@foobar.co (bla bla bla)",
		"Web:\n|www.foobar.co|\nE-Mail:\n      |bar@foobar.co| (bla bla bla)",
	)
}

func TestItUrl_uri_with_empty_path_and_with_query(t *testing.T) {
	itUrlAssertLinked(t,
		"upi://pay?pa=XXXXXXX&pn=XXXXX",
		"|upi://pay?pa=XXXXXXX&pn=XXXXX|",
	)
	itUrlAssertLinked(t,
		"https://example.org?pa=XXXXXXX&pn=XXXXX",
		"|https://example.org?pa=XXXXXXX&pn=XXXXX|",
	)
}

func TestItUrl_fuzz(t *testing.T) {
	itUrlAssertNotLinked(t, "ab:/ϸ")
}

func itUrlAssertNotLinked(t *testing.T, s string) {
	t.Helper()
	itUrlAssertLinked(t, s, s)
}

// Assert link with protocol
func itUrlAssertLinked(t *testing.T, input string, expected string) {
	t.Helper()
	finder := linkify.LinkFinderNew()
	itCommonAssertLinkedWith(t, finder, input, expected)
}

func itUrlAssertNotLinkedWithoutProtocol(t *testing.T, s string) {
	t.Helper()
	itUrlAssertUrlsWithoutProtocol(t, s, s)
}

// Assert link without protocol
func itUrlAssertUrlsWithoutProtocol(t *testing.T, input string, expected string) {
	t.Helper()
	finder := linkify.LinkFinderNew()
	finder.UrlMustHaveScheme(false)
	finder.Kinds([]linkify.LinkKind{linkify.LinkKind_Url})
	itCommonAssertLinkedWith(t, finder, input, expected)
}
