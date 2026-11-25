// Port of brokk-sessionwiki 0.34.28 src/util.rs.

package brokksessionwiki

import "golang.org/x/text/unicode/norm"

// NFC is s in Unicode Normalization Form C.
func NFC(s string) string { return norm.NFC.String(s) }
