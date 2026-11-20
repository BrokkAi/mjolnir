// Port of mj-core/src/hex.rs.

package core

import "encoding/hex"

// LowerHex is the lowercase hexadecimal form of b.
func LowerHex(b []byte) string { return hex.EncodeToString(b) }
