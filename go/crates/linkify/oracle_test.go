package linkify

import (
	"encoding/json"
	"math/rand/v2"
	"os"
	"strconv"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
)

// The differential test asks the real crate (go/port/oracle, module
// `linkify`) which links and spans it finds in a text and holds this package
// to the same answer. The corpus is recorded in testdata/oracle.
//
// MJ_ORACLE_SCALE multiplies the number of generated cases. It is for a
// longer run against the live oracle (`MJ_ORACLE=live MJ_ORACLE_SCALE=50
// ...`); the recorded corpus is made with the default of 1.
func oracleScale() int {
	scale, err := strconv.Atoi(os.Getenv("MJ_ORACLE_SCALE"))
	if err != nil || scale < 1 {
		return 1
	}
	return scale
}

type oracleRequest struct {
	Op                     string   `json:"op"`
	Text                   string   `json:"text"`
	Kinds                  []string `json:"kinds"`
	UrlMustHaveScheme      *bool    `json:"url_must_have_scheme"`
	UrlCanBeIri            *bool    `json:"url_can_be_iri"`
	EmailDomainMustHaveDot *bool    `json:"email_domain_must_have_dot"`
}

func oracleKindName(kind LinkKind) string {
	switch kind {
	case LinkKind_Url:
		return "url"
	case LinkKind_Email:
		return "email"
	default:
		return "other"
	}
}

func oracleAnswer(t *testing.T) func(json.RawMessage) any {
	return func(raw json.RawMessage) any {
		var request oracleRequest
		if err := json.Unmarshal(raw, &request); err != nil {
			t.Fatalf("decode %s: %v", raw, err)
		}
		if request.Op != "find" {
			t.Fatalf("unknown op %q", request.Op)
		}
		return oracle.Catch(func() any {
			finder := LinkFinderNew()
			if request.Kinds != nil {
				kinds := []LinkKind{}
				for _, kind := range request.Kinds {
					if kind == "email" {
						kinds = append(kinds, LinkKind_Email)
					} else {
						kinds = append(kinds, LinkKind_Url)
					}
				}
				finder.Kinds(kinds)
			}
			if request.UrlMustHaveScheme != nil {
				finder.UrlMustHaveScheme(*request.UrlMustHaveScheme)
			}
			if request.UrlCanBeIri != nil {
				finder.UrlCanBeIri(*request.UrlCanBeIri)
			}
			if request.EmailDomainMustHaveDot != nil {
				finder.EmailDomainMustHaveDot(*request.EmailDomainMustHaveDot)
			}

			links := []any{}
			for link := range finder.Links(request.Text).All() {
				links = append(links, []any{link.Start(), link.End(), oracleKindName(link.Kind()), link.AsStr()})
			}
			spans := []any{}
			var shown strings.Builder
			iterator := finder.Spans(request.Text)
			for {
				span, ok := iterator.Next().Get()
				if !ok {
					break
				}
				var kind any
				if linkKind, isLink := span.Kind().Get(); isLink {
					kind = oracleKindName(linkKind)
					shown.WriteString("|" + span.AsStr() + "|")
				} else {
					shown.WriteString(span.AsStr())
				}
				spans = append(spans, []any{span.Start(), span.End(), kind, len(span.AsStr())})
			}
			return map[string]any{"links": links, "spans": spans, "shown": shown.String()}
		})
	}
}

var oracleSchemes = []string{
	"http", "https", "ftp", "ssh", "file", "mailto", "a", "x-y", "a.b", "a+b", "ABC", "1abc", "a1", "a1b", "1",
	"", "git+ssh", "h2", "x_y", "é", "tel", "upi", "test", "facetime", "HTTP", "Https", "-", "+", ".", "a-", "ws",
}

var oracleSeparators = []string{"://", "://", "://", "://", ":", ":/", "//", ":///", "::", ":// ", "://:"}

var oracleAuthorities = []string{
	"example.com", "example.org", "localhost", "127.0.0.1", "1.2.3", "1.0.0.0", "1.0.0.0.0", "256.1.1.1", "12-7.0.0.1",
	"üñîçøðé.com", "xn--c1h.example.com", "a.b-c.co", "-a.com", "a-.com", "a.-b.com", "a.b-.com", "a..com", ".a.com",
	"a.com.", "a.com..", "user:pass@example.com", "user@host", "u:p@h:80", "a@b@c.com", "host:8080", "host:", "host:80:90",
	"[::1]", "[::1]:80", "example.c", "example.co", "e.com", "v1.1", "v1.1.1", "foo_bar.com", "foo~bar.com", "a!b.com",
	"a$b", "a'b.com", "(a.com)", "www.example.com", "sub.sub.example.museum", "EXAMPLE.COM", "日本.jp", "example.日本",
	"\u0080.com", "\u0081.com", "a.\u0080b", "a.\u0081b", "+19995551234", "123'456", "", "a", "ab", "g.cn", "9292.nl", "a12.b-c.com",
	"user:-.!$@example.com", "user:!$&'()*+,;=@example.com", "example.org'a@example.com", "example.com-", "a.b.c.d.e.fg",
	"1.2.3.4:5", "1.2.3.x", "x.1.2.3", "a.b1", "a.1b", "a.bc1", "com", "a.co.uk", "a.c0m", "..", ".", "a.", "😀.com", "a.😀",
}

var oraclePaths = []string{
	"", "", "/", "/a", "/a/b/", "/a(b)", "/a(b", "/a)b", "/a(b))", "/a[b]", "/a]b", "/a[b", "/a{b}", "/a}b", "/a{b",
	"/a'b'", "/a'b", "/it's", "/''", "/'''", "/\"a", "/`a", "/a|b", "/a b", "/<", "/>", "/a.", "/a..", "/a,", "/a:", "/a;",
	"/a!", "/a?", "/a*", "/!/", "/./", "/,/", "/?/", "/;/", "/:/", "/*/a", "?q=1", "?q=1&x=y", "/?foo=%20", "/#frag", "#frag",
	"/\u00a1", "/\u00a2/", "/ä", "/\U0001F600", "/\u009f", "/\u007f", "/\u0000", "/\u000e", "/a\\b", "/a^b", "/~user",
	"/a_b", "//", "/a//b", ":8080/x", "/x@y", "/a=b;c", "/%3C", "/é/é", "/é!/", "/\u00a2!", "/a\u0080", "/a\u0085b",
	"/a\u00a0b", "/a\u2028b", "/wiki/Link_(The_Legend_of_Zelda)", "/a.b.c", "/a,b", "/a;b:c", "/(", "/(.", "/]()", "/!",
	"?", "?a(b)", "/a?b?c", "/-", "/+", "/a\tb", "/a\nb", "/a\rb", "/a\u000bb",
}

var oracleWrappers = [][2]string{
	{"", ""}, {"(", ")"}, {"[", "]"}, {"{", "}"}, {"<", ">"}, {"\"", "\""}, {"'", "'"}, {"`", "`"}, {"((", "))"},
	{"[(", ")]"}, {"", "."}, {"", ","}, {"", ";"}, {"", ":"}, {"", "!"}, {"", "?"}, {"", "..."}, {"", ")."}, {"(", ".)"},
	{"'", ""}, {"\"", ""}, {"", "'"}, {"", "\""}, {"''", "''"}, {"'", "''"}, {"<a href=\"", "\">"}, {"<a href='", "'>"},
	{"[text](", ")"}, {"![img](", ")"}, {"*", "*"}, {"**", "**"}, {"_", "_"}, {"~", "~"}, {"|", "|"}, {"=", ""},
	{"\u00a1", ""}, {"é", "é"}, {"1", ""}, {"a", ""}, {".", ""}, {"1.", ""}, {"-", "-"}, {"@", ""}, {"/", ""}, {":", ":"},
	{"x@", ""}, {"", "@x"}, {"", "\">"}, {"", "'/>"}, {"", "<p>"}, {"", "\");"}, {"", "');"}, {" ", " "}, {"\t", "\n"},
	{"\u00a0", "\u00a0"}, {"\u2028", "\u2028"}, {"“", "”"}, {"«", "»"}, {"，", "。"}, {"\\", "\\"}, {"^", "^"}, {"#", "#"},
	{"%", "%"}, {"&", "&"}, {"+", "+"}, {"$", "$"}, {",", ","}, {";", ";"}, {"?", "?"}, {"!", "!"}, {"0", "0"},
}

var oracleLocalParts = []string{
	"a", "foo", "foo.bar", ".foo", "foo.", "a..b", "#!$%&'*+-/=?^_`{}|~", "üñîçøðé", "www", "me@a", "", "a b", "\"a\"",
	"a,b", "a:b", "a;b", "(a", "a)", "<a", "é.é", "1", "a/b", "x.y.z",
}

var oracleLeads = []string{"", "", "", "see ", "Visit ", "foo\n", "\t", "a b ", "http://one.org/ ", "x@y.zz ", "one.org/ ", "Web:\n", "E-Mail:\n      "}

var oracleTrails = []string{"", "", "", " ok", "\nnext", " http://two.org/", " a@b.co", " two.org/", " (bla bla bla)", ".", " : ", "(http://two.org/)"}

// oracleSoup is the alphabet of the unstructured cases: every character the
// scanners branch on, and a few letters and digits to put between them.
var oracleSoup = []string{
	":", "/", "/", "@", ".", ".", "-", "_", "~", "!", "$", "&", "'", "(", ")", "*", "+", ",", ";", "=", "?", "#", "[", "]",
	"{", "}", "<", ">", "\"", "`", "|", " ", "\n", "\t", "\\", "^", "%", "a", "b", "c", "z", "A", "Z", "0", "1", "9",
	"://", "://", "http", "www", "com", "co", "é", "日", "😀", "\u0080", "\u0081", "\u009f", "\u00a0", "\u007f", "\u0000",
	"\u001f", "\u2028", "\ufffd",
}

func oracleRandom(seed uint64) *rand.Rand {
	return rand.New(rand.NewPCG(seed, 0x6c696e6b696679))
}

func oraclePick(random *rand.Rand, items []string) string {
	return items[random.IntN(len(items))]
}

// oracleConfigure adds one of the finder configurations to a request. Index
// 0 is the default finder; index 1 is what Mjolnir uses (URLs only).
func oracleConfigure(request map[string]any, index int) map[string]any {
	switch index % 12 {
	case 0:
	case 1:
		request["kinds"] = []string{"url"}
	case 2:
		request["url_must_have_scheme"] = false
	case 3:
		request["url_must_have_scheme"] = false
		request["kinds"] = []string{"url"}
	case 4:
		request["url_can_be_iri"] = false
	case 5:
		request["url_can_be_iri"] = false
		request["url_must_have_scheme"] = false
	case 6:
		request["kinds"] = []string{"email"}
	case 7:
		request["email_domain_must_have_dot"] = false
	case 8:
		request["kinds"] = []string{"email"}
		request["email_domain_must_have_dot"] = false
		request["url_can_be_iri"] = false
	case 9:
		request["kinds"] = []string{}
	case 10:
		request["kinds"] = []string{"url", "email"}
		request["url_must_have_scheme"] = false
		request["email_domain_must_have_dot"] = false
		request["url_can_be_iri"] = false
	case 11:
		request["kinds"] = []string{"url"}
		request["url_can_be_iri"] = false
	}
	return request
}

func oracleFind(text string, config int) map[string]any {
	return oracleConfigure(map[string]any{"op": "find", "text": text}, config)
}

func TestFindAgainstRust(t *testing.T) {
	oracle.Check(t, "linkify", "find", func(yield func(any)) {
		fixed := []string{
			"",
			"Have you seen http://example.com?",
			"Look, no scheme: example.org/foo",
			"http://example.com and foo@example.com",
			"https://en.wikipedia.org/wiki/Link_(The_Legend_of_Zelda)",
			"http://üñîçøðé.com/ä",
			"<http://example.com/>",
			"(http://example.com/)",
			"http://example.com/.",
			"http://example.com/,",
			"Web:\nwww.foobar.co\nE-Mail:\n      bar@foobar.co (bla bla bla)",
			"see [the docs](https://example.com/docs/a_b#frag), then `http://localhost:8080/x?y=1&z=2`.",
			"mailto:someone@example.com, tel:+15551234, file:///etc/hosts and ssh://git@github.com/BrokkAi/mjolnir.git",
			"a://b b://c c://d",
			"http://a.b/c http://a.b/c",
			"www@example.com",
			"http://example.org/it's and 'http://example.org/' and \"http://example.org/\"",
			"::::@@@@....////",
			"a@b.c a@b.cd a@b.c1 a@1.2.3.4 a@1.2.3",
			"x.y x.yz 1.2.3.4 1.2.3 a.b.c.de",
		}
		for _, text := range fixed {
			for config := range 12 {
				yield(oracleFind(text, config))
			}
		}

		// One well-formed link in every delimiter context.
		links := []string{
			"http://example.org/", "https://example.com", "example.org/a", "www.example.com", "a@example.com",
			"file:///x", "http://example.org/a(b)", "http://üñîçøðé.com/ä", "http://127.0.0.1:8080/a?b=c#d",
			"https://user:pass@example.com/path/", "test://123'456", "ftp://a.b-c.co/a,b;c",
		}
		index := 0
		for _, wrapper := range oracleWrappers {
			for _, link := range links {
				text := wrapper[0] + link + wrapper[1]
				yield(oracleFind(text, 1))
				yield(oracleFind("see "+text+" and more", index))
				index++
			}
		}

		// Every scheme, separator and authority at least once.
		for _, scheme := range oracleSchemes {
			for _, separator := range oracleSeparators {
				yield(oracleFind(scheme+separator+"example.org/x y", index))
				index++
			}
		}
		for _, authority := range oracleAuthorities {
			yield(oracleFind("http://"+authority, 0))
			yield(oracleFind("x://"+authority+"/a", index))
			yield(oracleFind(authority, 3))
			yield(oracleFind(authority+"/p.", 2))
			yield(oracleFind("me@"+authority, 6))
			yield(oracleFind("me@"+authority+".", 7))
			index++
		}
		for _, path := range oraclePaths {
			yield(oracleFind("http://example.org"+path, 0))
			yield(oracleFind("\"http://example.org"+path+"\"", 1))
			yield(oracleFind("'example.org"+path+"' x", 3))
			yield(oracleFind("http://example.org"+path+" z", 11))
			index++
		}
		for _, local := range oracleLocalParts {
			yield(oracleFind(local+"@example.com", 6))
			yield(oracleFind("x "+local+"@b y", 7))
			yield(oracleFind(local+"@example.com", 2))
		}

		random := oracleRandom(1)
		// Links assembled from the parts above, in a random context.
		for range 1300 * oracleScale() {
			var text strings.Builder
			text.WriteString(oraclePick(random, oracleLeads))
			for range 1 + random.IntN(2) {
				wrapper := oracleWrappers[random.IntN(len(oracleWrappers))]
				text.WriteString(wrapper[0])
				switch random.IntN(5) {
				case 0:
					text.WriteString(oraclePick(random, oracleLocalParts) + "@" + oraclePick(random, oracleAuthorities))
				case 1:
					text.WriteString(oraclePick(random, oracleAuthorities) + oraclePick(random, oraclePaths))
				default:
					text.WriteString(oraclePick(random, oracleSchemes) + oraclePick(random, oracleSeparators) +
						oraclePick(random, oracleAuthorities) + oraclePick(random, oraclePaths))
				}
				text.WriteString(wrapper[1])
				if random.IntN(3) == 0 {
					text.WriteString(" ")
				}
			}
			text.WriteString(oraclePick(random, oracleTrails))
			yield(oracleFind(text.String(), random.IntN(12)))
		}
		// Unstructured text made of the characters the scanners branch on.
		for range 1500 * oracleScale() {
			var text strings.Builder
			for range 1 + random.IntN(18) {
				text.WriteString(oraclePick(random, oracleSoup))
			}
			yield(oracleFind(text.String(), random.IntN(12)))
		}
	}, oracleAnswer(t))
}

// A Rust `&str` is always UTF-8 and a Go string need not be. Whatever the
// bytes, the finder must not panic and the spans must cover the text exactly
// once.
func TestSpansCoverAnyBytes(t *testing.T) {
	random := oracleRandom(2)
	alphabet := []byte(":/@.-'\"()a1 \x80\xc3\xa9\xff\xf0\x9f\xe2")
	for range 20000 {
		raw := make([]byte, random.IntN(24))
		for i := range raw {
			raw[i] = alphabet[random.IntN(len(alphabet))]
		}
		text := string(raw)
		for config := range 12 {
			finder := LinkFinderNew()
			request := oracleFind(text, config)
			if kinds, ok := request["kinds"].([]string); ok {
				list := []LinkKind{}
				for _, kind := range kinds {
					if kind == "email" {
						list = append(list, LinkKind_Email)
					} else {
						list = append(list, LinkKind_Url)
					}
				}
				finder.Kinds(list)
			}
			if value, ok := request["url_must_have_scheme"].(bool); ok {
				finder.UrlMustHaveScheme(value)
			}
			if value, ok := request["url_can_be_iri"].(bool); ok {
				finder.UrlCanBeIri(value)
			}
			if value, ok := request["email_domain_must_have_dot"].(bool); ok {
				finder.EmailDomainMustHaveDot(value)
			}
			position := 0
			var joined strings.Builder
			for span := range finder.Spans(text).All() {
				if span.Start() != position || span.End() <= span.Start() {
					t.Fatalf("%q (config %d): span %d..%d after position %d", text, config, span.Start(), span.End(), position)
				}
				position = span.End()
				joined.WriteString(span.AsStr())
			}
			if joined.String() != text {
				t.Fatalf("%q (config %d): spans join to %q", text, config, joined.String())
			}
		}
	}
}
