// Port of pulldown-cmark 0.10.3 src/puncttable.rs

package pulldowncmark

import "testing"

func TestPuncttable_test_ascii(t *testing.T) {
	assert(t, puncttableIsAsciiPunctuation('!'))
	assert(t, puncttableIsAsciiPunctuation('@'))
	assert(t, puncttableIsAsciiPunctuation('~'))
	assert(t, !puncttableIsAsciiPunctuation(' '))
	assert(t, !puncttableIsAsciiPunctuation('0'))
	assert(t, !puncttableIsAsciiPunctuation('A'))
	assert(t, !puncttableIsAsciiPunctuation(0xA1))
}

func TestPuncttable_test_unicode(t *testing.T) {
	assert(t, puncttableIsPunctuation('~'))
	assert(t, !puncttableIsPunctuation(' '))

	assert(t, puncttableIsPunctuation('¡'))
	assert(t, puncttableIsPunctuation('،'))
	assert(t, puncttableIsPunctuation('･'))
	assert(t, puncttableIsPunctuation('\U0001BC9F'))
	assert(t, !puncttableIsPunctuation('\U0001BCA0'))
}
