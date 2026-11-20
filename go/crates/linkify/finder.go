// Port of linkify 0.11.0 src/finder.rs

package linkify

import (
	"iter"
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// A link found in the input text.
//
// A Link is a small read-only record, and its methods have value receivers,
// so that a call can follow `Next().Unwrap()` directly as it does in Rust.
type Link struct {
	text  string   `rs:"text"`
	start int      `rs:"start"`
	end   int      `rs:"end"`
	kind  LinkKind `rs:"kind"`
}

var _ = rs.RegisterStruct[Link]("Link")

// The start index of the link within the input text.
func (self Link) Start() int {
	return self.start
}

// The end index of the link.
func (self Link) End() int {
	return self.end
}

// Get the link text as a `str`.
func (self Link) AsStr() string {
	return self.text[self.start:self.end]
}

// The type of the link.
func (self Link) Kind() LinkKind {
	return self.kind
}

// The type of link that was found.
type LinkKind uint8

const (
	// URL links like "http://example.org".
	LinkKind_Url LinkKind = iota
	// E-mail links like "foo@example.org"
	LinkKind_Email
)

var _ = rs.RegisterEnum[LinkKind]("LinkKind",
	rs.Const(LinkKind_Url, "Url"),
	rs.Const(LinkKind_Email, "Email"),
)

// Span within the input text.
//
// A span represents a substring of the input text,
// which can either be a link, or plain text.
//
// Like Link, a Span is a read-only record with value receivers.
type Span struct {
	text  string              `rs:"text"`
	start int                 `rs:"start"`
	end   int                 `rs:"end"`
	kind  rs.Option[LinkKind] `rs:"kind"`
}

var _ = rs.RegisterStruct[Span]("Span")

// The start index of the span within the input text.
func (self Span) Start() int {
	return self.start
}

// The end index of the span.
func (self Span) End() int {
	return self.end
}

// Get the span text as a `str`.
func (self Span) AsStr() string {
	return self.text[self.start:self.end]
}

// The type of link included in the span, if any.
//
// Returns `None` if the span represents plain text.
func (self Span) Kind() rs.Option[LinkKind] {
	return self.kind
}

// A configured link finder.
type LinkFinder struct {
	email                  bool `rs:"email"`
	emailDomainMustHaveDot bool `rs:"email_domain_must_have_dot"`
	url                    bool `rs:"url"`
	urlMustHaveScheme      bool `rs:"url_must_have_scheme"`
	urlCanBeIri            bool `rs:"url_can_be_iri"`
}

var _ = rs.RegisterStruct[LinkFinder]("LinkFinder")

// finderTriggerFinder is `dyn Fn(&[u8]) -> Option<usize>`: where the next
// character that can start a scan is. It takes the text as a string, which
// in Go is the same bytes.
type finderTriggerFinder func(s string) rs.Option[int]

// Iterator for finding links.
type Links struct {
	text   string
	rewind int

	triggerFinder finderTriggerFinder
	emailScanner  emailEmailScanner
	urlScanner    urlUrlScanner
	domainScanner urlDomainScanner
}

// Iterator over spans.
type Spans struct {
	text     string
	position int
	links    finderPeekable
}

// finderPeekable is `Peekable<Links<'t>>`.
type finderPeekable struct {
	iter   *Links
	peeked rs.Option[rs.Option[Link]]
}

// peek is `Peekable::peek`: a pointer to the next link, or nil when there is
// none, without moving past it.
func (self *finderPeekable) peek() *Link {
	if self.peeked.IsNone() {
		self.peeked = rs.Some(self.iter.Next())
	}
	return self.peeked.AsRef().AsRef()
}

// next is `Peekable::next`.
func (self *finderPeekable) next() rs.Option[Link] {
	if peeked, ok := self.peeked.Take().Get(); ok {
		return peeked
	}
	return self.iter.Next()
}

// Create a new link finder with the default options for finding all kinds
// of links.
//
// If you only want to find a certain kind of links, use the `kinds` method.
func LinkFinderNew() *LinkFinder {
	return &LinkFinder{
		email:                  true,
		emailDomainMustHaveDot: true,
		url:                    true,
		urlMustHaveScheme:      true,
		urlCanBeIri:            true,
	}
}

// Require the domain parts of email addresses to have at least one dot.
// Use `false` to also find addresses such as `root@localhost`.
func (self *LinkFinder) EmailDomainMustHaveDot(value bool) *LinkFinder {
	self.emailDomainMustHaveDot = value
	return self
}

// Set whether URLs must have a scheme, defaults to `true`.
//
// By default only URLs having a scheme defined are found.
// By setting this to `false` you make the scheme of URLs optional, to also find URLs like
// `example.org`. For some URLs the used scheme is important, and making the scheme optional
// may lead to finding a lot of false positive URLs.
func (self *LinkFinder) UrlMustHaveScheme(urlMustHaveScheme bool) *LinkFinder {
	self.urlMustHaveScheme = urlMustHaveScheme
	return self
}

// Sets whether URLs can be IRI according to RFC-3987.
// The default is `true`.
// Setting it to `false` means domains can contain ASCII characters only.
func (self *LinkFinder) UrlCanBeIri(urlCanBeIri bool) *LinkFinder {
	self.urlCanBeIri = urlCanBeIri
	return self
}

// Restrict the kinds of links that should be found to the specified ones.
func (self *LinkFinder) Kinds(kinds []LinkKind) *LinkFinder {
	self.email = false
	self.url = false
	for _, kind := range kinds {
		switch kind {
		case LinkKind_Email:
			self.email = true
		case LinkKind_Url:
			self.url = true
		default:
			panic(rs.Unreachable(kind))
		}
	}
	return self
}

// Find links in the specified input text.
//
// Returns an `Iterator` which only scans when `next` is called (lazy).
func (self *LinkFinder) Links(text string) *Links {
	return linksNew(
		text,
		self.url,
		self.urlMustHaveScheme,
		self.email,
		self.emailDomainMustHaveDot,
		self.urlCanBeIri,
	)
}

// Iterate over spans in the specified input text.
//
// A span represents a substring of the input text,
// which can either be a link, or plain text.
//
// Returns an `Iterator` which only scans when `next` is called (lazy).
//
// The spans that are returned by the `Iterator` are consecutive,
// and when combined represent the input text in its entirety.
func (self *LinkFinder) Spans(text string) *Spans {
	return &Spans{
		text:     text,
		position: 0,
		links: finderPeekable{
			iter:   self.Links(text),
			peeked: rs.None[rs.Option[Link]](),
		},
	}
}

// LinkFinderDefault is `impl Default for LinkFinder`.
func LinkFinderDefault() *LinkFinder {
	return LinkFinderNew()
}

// finderMemchr is `memchr::memchr`, `memchr2` and `memchr3`: the index of
// the first byte of s that is one of needles.
func finderMemchr(s string, needles string) rs.Option[int] {
	var index int
	if len(needles) == 1 {
		index = strings.IndexByte(s, needles[0])
	} else {
		index = strings.IndexAny(s, needles)
	}
	if index < 0 {
		return rs.None[int]()
	}
	return rs.Some(index)
}

func linksNew(
	text string,
	url bool,
	urlMustHaveScheme bool,
	email bool,
	emailDomainMustHaveDot bool,
	iriParsingEnabled bool,
) *Links {
	urlScanner := urlUrlScanner{
		iriParsingEnabled: iriParsingEnabled,
	}
	domainScanner := urlDomainScanner{
		iriParsingEnabled: iriParsingEnabled,
	}
	emailScanner := emailEmailScanner{
		domainMustHaveDot: emailDomainMustHaveDot,
	}

	// With optional schemes URLs don't have unique `:`, then search for `.` as well
	var triggerFinder finderTriggerFinder
	switch {
	case url && email && urlMustHaveScheme:
		triggerFinder = func(s string) rs.Option[int] { return finderMemchr(s, ":@") }
	case url && email:
		triggerFinder = func(s string) rs.Option[int] { return finderMemchr(s, ":@.") }
	case url && !email && urlMustHaveScheme:
		triggerFinder = func(s string) rs.Option[int] { return finderMemchr(s, ":") }
	case url && !email:
		triggerFinder = func(s string) rs.Option[int] { return finderMemchr(s, ":.") }
	case !url && email:
		triggerFinder = func(s string) rs.Option[int] { return finderMemchr(s, "@") }
	default:
		triggerFinder = func(string) rs.Option[int] { return rs.None[int]() }
	}
	return &Links{
		text:          text,
		rewind:        0,
		triggerFinder: triggerFinder,
		emailScanner:  emailScanner,
		urlScanner:    urlScanner,
		domainScanner: domainScanner,
	}
}

// Next is `Iterator::next`.
func (self *Links) Next() rs.Option[Link] {
	slice := self.text[self.rewind:]

	findFrom := 0
	for {
		i, ok := self.triggerFinder(slice[findFrom:]).Get()
		if !ok {
			break
		}
		trigger := slice[findFrom+i]
		var scanner scannerScanner
		var kind LinkKind
		switch trigger {
		case ':':
			scanner, kind = &self.urlScanner, LinkKind_Url
		case '.':
			scanner, kind = &self.domainScanner, LinkKind_Url
		case '@':
			scanner, kind = &self.emailScanner, LinkKind_Email
		default:
			panic(rs.Unreachable())
		}
		if range_, ok := scanner.scan(slice, findFrom+i).Get(); ok {
			start := self.rewind + range_.start
			end := self.rewind + range_.end
			self.rewind = end
			link := Link{
				text:  self.text,
				start: start,
				end:   end,
				kind:  kind,
			}
			return rs.Some(link)
		} else {
			// The scanner didn't find anything. But there could be more
			// trigger characters later, so continue the search.
			findFrom += i + 1
		}
	}

	return rs.None[Link]()
}

// All returns the rest of the iterator as a sequence to range over. It is
// what a Rust `for link in finder.links(text)` loop does.
func (self *Links) All() iter.Seq[Link] {
	return func(yield func(Link) bool) {
		for {
			link, ok := self.Next().Get()
			if !ok || !yield(link) {
				return
			}
		}
	}
}

// Collect is `Iterator::collect::<Vec<_>>()`: the rest of the iterator.
func (self *Links) Collect() []Link {
	rv := []Link{}
	for link := range self.All() {
		rv = append(rv, link)
	}
	return rv
}

// FmtDebug is `impl fmt::Debug for Links`.
func (self *Links) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("Links").Field("text", self.text).Finish()
}

// Next is `Iterator::next`.
func (self *Spans) Next() rs.Option[Span] {
	if link := self.links.peek(); link != nil {
		if self.position < link.start {
			span := Span{
				text:  self.text,
				start: self.position,
				end:   link.start,
				kind:  rs.None[LinkKind](),
			}
			self.position = link.start
			return rs.Some(span)
		}
	} else {
		if self.position < len(self.text) {
			span := Span{
				text:  self.text,
				start: self.position,
				end:   len(self.text),
				kind:  rs.None[LinkKind](),
			}
			self.position = len(self.text)
			return rs.Some(span)
		}
	}
	return rs.OptionMap(self.links.next(), func(link Link) Span {
		self.position = link.end
		return Span{
			text:  self.text,
			start: link.start,
			end:   link.end,
			kind:  rs.Some(link.kind),
		}
	})
}

// All returns the rest of the iterator as a sequence to range over. It is
// what a Rust `for span in finder.spans(text)` loop does.
func (self *Spans) All() iter.Seq[Span] {
	return func(yield func(Span) bool) {
		for {
			span, ok := self.Next().Get()
			if !ok || !yield(span) {
				return
			}
		}
	}
}

// Collect is `Iterator::collect::<Vec<_>>()`: the rest of the iterator.
func (self *Spans) Collect() []Span {
	rv := []Span{}
	for span := range self.All() {
		rv = append(rv, span)
	}
	return rv
}

// FmtDebug is `impl fmt::Debug for Spans`.
func (self *Spans) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("Spans").Field("text", self.text).Finish()
}
