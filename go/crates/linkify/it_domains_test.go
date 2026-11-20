// Port of linkify 0.11.0 tests/domains.rs

package linkify_test

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/linkify"
)

func TestItDomains_domain_valid(t *testing.T) {
	itDomainsAssertLinked(t, "9292.nl", "|9292.nl|")
	itDomainsAssertLinked(t, "a12.b-c.com", "|a12.b-c.com|")
}

func TestItDomains_domain_invalid_tld(t *testing.T) {
	itDomainsAssertNotLinked(t, "v1.2.3")
	itDomainsAssertNotLinked(t, "https://12-7.0.0.1/")
}

func TestItDomains_domain_with_userinfo(t *testing.T) {
	itDomainsAssertLinked(t,
		"https://user:pass@example.com/",
		"|https://user:pass@example.com/|",
	)
	itDomainsAssertLinked(t,
		"https://user:-.!$@example.com/",
		"|https://user:-.!$@example.com/|",
	)
	itDomainsAssertLinked(t,
		"https://user:!$&'()*+,;=@example.com/",
		"|https://user:!$&'()*+,;=@example.com/|",
	)

	// Can't have another @
	itDomainsAssertNotLinked(t, "https://user:pass@ex@mple.com/")
}

func TestItDomains_domain_with_port(t *testing.T) {
	itDomainsAssertLinked(t, "https://localhost:8080!", "|https://localhost:8080|!")
	itDomainsAssertLinked(t, "https://localhost:8080/", "|https://localhost:8080/|")
}

func TestItDomains_domain_with_userinfo_and_port(t *testing.T) {
	itDomainsAssertLinked(t,
		"https://user:pass@example.com:8080/hi",
		"|https://user:pass@example.com:8080/hi|",
	)
}

func TestItDomains_domain_ipv4(t *testing.T) {
	itDomainsAssertLinked(t, "https://127.0.0.1/", "|https://127.0.0.1/|")
	itDomainsAssertLinked(t, "1.0.0.0", "|1.0.0.0|")
	itDomainsAssertLinked(t, "1.0.0.0/foo/bar", "|1.0.0.0/foo/bar|")
	itDomainsAssertNotLinked(t, "1.0 ")
	itDomainsAssertNotLinked(t, "1.0.0")
	itDomainsAssertNotLinked(t, "1.0.0.0.0")
	itDomainsAssertNotLinked(t, "1.0.0.")
}

func TestItDomains_domain_trailing_dot(t *testing.T) {
	// itDomainsAssertLinked(t, "https://example.com./test", "|https://example.com./test|")
	itDomainsAssertLinked(t,
		"https://example.com.:8080/test",
		"|https://example.com.:8080/test|",
	)
}

func TestItDomains_domain_delimited(t *testing.T) {
	// Delimiter at end of domain should *not* be included
	itDomainsAssertLinked(t, "https://example.org'", "|https://example.org|'")
	// Unless it's a userinfo of course (sike!)
	itDomainsAssertLinked(t,
		"https://example.org'a@example.com",
		"|https://example.org'a@example.com|",
	)
}

func TestItDomains_domain_delimited_multiple(t *testing.T) {
	itDomainsAssertLinked(t,
		"https://a.com'https://b.com",
		"https://a.com'|https://b.com|",
	)
}

func TestItDomains_domain_dots(t *testing.T) {
	itDomainsAssertLinked(t, "https://example.com...", "|https://example.com|...")
}

func TestItDomains_domain_labels_cant_be_empty(t *testing.T) {
	itDomainsAssertNotLinked(t, "www.example..com")
	itDomainsAssertNotLinked(t, "https://.www.example.com")
}

func TestItDomains_domain_labels_cant_start_with_hyphen(t *testing.T) {
	itDomainsAssertNotLinked(t, "-a.com")
	itDomainsAssertNotLinked(t, "https://a.-b.com")
}

func TestItDomains_domain_labels_cant_end_with_hyphen(t *testing.T) {
	itDomainsAssertNotLinked(t, "a-.com")
	itDomainsAssertNotLinked(t, "a.b-.com")

	itDomainsAssertNotLinked(t, "https://a.b-.com")
	// Could also argue that it should not be linked at all
	itDomainsAssertLinked(t, "https://example.com-/", "|https://example.com|-/")
	itDomainsAssertLinked(t, "https://example.org-", "|https://example.org|-")
}

func TestItDomains_domain_cant_contain_at(t *testing.T) {
	// Looks like an email but was recognized as a schemeless link before.
	itDomainsAssertNotLinked(t, "example.com@about")
	// As part of path it's ok.
	itDomainsAssertLinked(t, "example.com/@about", "|example.com/@about|")
	itDomainsAssertLinked(t, "https://example.com/@about", "|https://example.com/@about|")
}

func TestItDomains_domain_cant_end_numeric(t *testing.T) {
	itDomainsAssertNotLinked(t, "info@v1.1.1")
}

func TestItDomains_no_authority_part(t *testing.T) {
	itDomainsAssertLinked(t, "file:///", "|file:///|")
	itDomainsAssertLinked(t, "file:///home/foo", "|file:///home/foo|")
	itDomainsAssertLinked(t, "file://localhost/home/foo", "|file://localhost/home/foo|")
}

func TestItDomains_authority_thats_not_domain(t *testing.T) {
	// Not valid according to DNS but we should allow it for other schemes (or all, not sure).
	itDomainsAssertLinked(t, "facetime://+19995551234", "|facetime://+19995551234|")
}

func TestItDomains_authority_without_slash_should_stop_at_delimiters(t *testing.T) {
	// What's going on here? For schemes where we don't enforce domainyness,
	// we want to stop at delimiter characters. Note that `!` is valid in authorities.
	itDomainsAssertLinked(t, "test://123'456!!!", "|test://123'456|!!!")
	itDomainsAssertLinked(t, "test://123'456...", "|test://123'456|...")
	// ... unless there is a "/" terminating the authority.
	itDomainsAssertLinked(t, "test://123'456!!!/", "|test://123'456!!!/|")
	itDomainsAssertLinked(t, "test://123'456.../", "|test://123'456.../|")
}

func TestItDomains_without_scheme_should_stop(t *testing.T) {
	// itDomainsAssertLinked(t, "ab/example.com", "ab/|example.com|")
	// This is not a valid scheme. Even the schemeless parser should not accept it, nor extract
	// only example.com out of it.
	itDomainsAssertNotLinked(t, "1abc://example.com")
}

func TestItDomains_test_international_not_allowed(t *testing.T) {
	finder := linkify.LinkFinderNew()
	finder.UrlMustHaveScheme(false)
	finder.UrlCanBeIri(false)
	link := finder.Links("\u00a1\u00a2example.com").Next().Unwrap()
	itCommonAssertEq(t, link.Kind(), linkify.LinkKind_Url)
	itCommonAssertEq(t, link.AsStr(), "example.com")
}

func TestItDomains_test_international_allowed(t *testing.T) {
	finder := linkify.LinkFinderNew()
	finder.UrlMustHaveScheme(false)
	finder.UrlCanBeIri(true)
	link := finder.Links("\u00a1\u00a2example.com").Next().Unwrap()
	itCommonAssertEq(t, link.Kind(), linkify.LinkKind_Url)
	itCommonAssertEq(t, link.AsStr(), "\u00a1\u00a2example.com")
}

func itDomainsAssertLinked(t *testing.T, input string, expected string) {
	t.Helper()
	finder := linkify.LinkFinderNew()
	finder.UrlMustHaveScheme(false)

	itCommonAssertLinkedWith(t, finder, input, expected)
}

func itDomainsAssertNotLinked(t *testing.T, s string) {
	t.Helper()
	itDomainsAssertLinked(t, s, s)
}
