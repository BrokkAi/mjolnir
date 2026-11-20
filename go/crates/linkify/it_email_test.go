// Port of linkify 0.11.0 tests/email.rs

package linkify_test

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/linkify"
)

func TestItEmail_no_links(t *testing.T) {
	itEmailAssertNotLinked(t, "")
	itEmailAssertNotLinked(t, "foo")
	itEmailAssertNotLinked(t, "@")
	itEmailAssertNotLinked(t, "a@")
	itEmailAssertNotLinked(t, "@a")
	itEmailAssertNotLinked(t, "@@@")
}

func TestItEmail_simple(t *testing.T) {
	itEmailAssertLinked(t, "foo@example.com", "|foo@example.com|")
	itEmailAssertLinked(t, "foo.bar@example.com", "|foo.bar@example.com|")
}

func TestItEmail_allowed_text(t *testing.T) {
	// I know, I know...
	itEmailAssertLinked(t,
		"#!$%&'*+-/=?^_`{}|~@example.org",
		"|#!$%&'*+-/=?^_`{}|~@example.org|",
	)
}

func TestItEmail_space_separation(t *testing.T) {
	itEmailAssertLinked(t, "foo a@b.com", "foo |a@b.com|")
	itEmailAssertLinked(t, "a@b.com foo", "|a@b.com| foo")
	itEmailAssertLinked(t, "\na@b.com", "\n|a@b.com|")
	itEmailAssertLinked(t, "a@b.com\n", "|a@b.com|\n")
}

func TestItEmail_special_separation(t *testing.T) {
	itEmailAssertLinked(t, "(a@example.com)", "(|a@example.com|)")
	itEmailAssertLinked(t, "\"a@example.com\"", "\"|a@example.com|\"")
	itEmailAssertLinked(t, "\"a@example.com\"", "\"|a@example.com|\"")
	itEmailAssertLinked(t, ",a@example.com,", ",|a@example.com|,")
	itEmailAssertLinked(t, ":a@example.com:", ":|a@example.com|:")
	itEmailAssertLinked(t, ";a@example.com;", ";|a@example.com|;")
}

func TestItEmail_dots(t *testing.T) {
	itEmailAssertNotLinked(t, ".@example.com")
	itEmailAssertNotLinked(t, "foo.@example.com")
	itEmailAssertLinked(t, ".foo@example.com", ".|foo@example.com|")
	itEmailAssertLinked(t, ".foo@example.com", ".|foo@example.com|")
	itEmailAssertLinked(t, "a..b@example.com", "a..|b@example.com|")
	itEmailAssertLinked(t, "a@example.com.", "|a@example.com|.")
}

func TestItEmail_domain_without_dot(t *testing.T) {
	itEmailAssertNotLinked(t, "a@b")
	itEmailAssertNotLinked(t, "a@b.")
	itEmailAssertLinked(t, "a@b.com.", "|a@b.com|.")
}

func TestItEmail_dashes(t *testing.T) {
	itEmailAssertLinked(t, "a@example.com-", "|a@example.com|-")
	itEmailAssertLinked(t, "a@foo-bar.com", "|a@foo-bar.com|")
	itEmailAssertNotLinked(t, "a@-foo.com")
	itEmailAssertNotLinked(t, "a@b-.")
}

func TestItEmail_domain_must_have_dot_false(t *testing.T) {
	finder := linkify.LinkFinderNew()
	finder.Kinds([]linkify.LinkKind{linkify.LinkKind_Email})
	finder.EmailDomainMustHaveDot(false)

	itCommonAssertLinkedWith(t, finder, "a@b", "|a@b|")
	itCommonAssertLinkedWith(t, finder, "a@b.", "|a@b|.")

	itCommonAssertLinkedWith(t, finder, "a@b.", "|a@b|.")
}

func TestItEmail_multiple(t *testing.T) {
	itEmailAssertLinked(t,
		"a@example.com b@example.com",
		"|a@example.com| |b@example.com|",
	)
	itEmailAssertLinked(t,
		"a@example.com @ b@example.com",
		"|a@example.com| @ |b@example.com|",
	)
}

func TestItEmail_multiple_delimited_hard(t *testing.T) {
	itEmailAssertLinked(t,
		"a@xy.com;b@xy.com,c@xy.com",
		"|a@xy.com|;|b@xy.com|,|c@xy.com|",
	)
}

func TestItEmail_international(t *testing.T) {
	itEmailAssertLinked(t, "üñîçøðé@example.com", "|üñîçøðé@example.com|")
	itEmailAssertLinked(t, "üñîçøðé@üñîçøðé.com", "|üñîçøðé@üñîçøðé.com|")
}

func TestItEmail_trigger_overlap(t *testing.T) {
	finder := linkify.LinkFinderNew()

	// 'w' is a trigger character for WWW links. Make sure we can rewind enough.
	itCommonAssertLinkedWith(t, finder, "www@example.com", "|www@example.com|")
}

func TestItEmail_fuzz(t *testing.T) {
	itEmailAssertLinked(t, "a@a.xyϸ", "|a@a.xyϸ|")
}

func itEmailAssertNotLinked(t *testing.T, s string) {
	t.Helper()
	finder := linkify.LinkFinderNew()
	finder.Kinds([]linkify.LinkKind{linkify.LinkKind_Email})
	result := finder.Links(s)
	if count := len(result.Collect()); count != 0 {
		t.Fatalf("assertion `left == right` failed: expected no links in %q\n  left: %d\n right: 0", s, count)
	}
}

func itEmailAssertLinked(t *testing.T, input string, expected string) {
	t.Helper()
	finder := linkify.LinkFinderNew()
	finder.Kinds([]linkify.LinkKind{linkify.LinkKind_Email})
	itCommonAssertLinkedWith(t, finder, input, expected)
}
