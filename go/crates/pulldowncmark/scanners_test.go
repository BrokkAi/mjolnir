// Port of pulldown-cmark 0.10.3 src/scanners.rs

package pulldowncmark

import "testing"

func TestScanners_overflow_list(t *testing.T) {
	_, _, _, _, ok := scannersScanListitem("4444444444444444444444444444444444444444444444444444444444!")
	assert(t, !ok)
}

func TestScanners_overflow_by_addition(t *testing.T) {
	_, _, _, _, ok := scannersScanListitem("1844674407370955161615!")
	assert(t, !ok)
}

func TestScanners_good_emails(t *testing.T) {
	emails := []string{
		"<a@b.c>",
		"<a@b>",
		"<a-zA-Z0-9.!#$%&'*+/=?^_`{|}~-@example.com>",
		"<a@sixty-three-letters-in-this-identifier-----------------------63>",
	}
	for _, email := range emails {
		_, _, ok := scannersScanEmail(email, 1)
		assert(t, ok)
	}
}

func TestScanners_bad_emails(t *testing.T) {
	emails := []string{
		"<@b.c>",
		"<foo@-example.com>",
		"<foo@example-.com>",
		"<a@notrailingperiod.>",
		"<a(noparens)@example.com>",
		"<\"noquotes\"@example.com>",
		"<a@sixty-four-letters-in-this-identifier-------------------------64>",
	}
	for _, email := range emails {
		_, _, ok := scannersScanEmail(email, 1)
		assert(t, !ok)
	}
}
