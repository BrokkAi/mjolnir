// Port of mj-server/src/server/api/files.rs (only the wire items so far).

package server

import (
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// WriteFileResponse is what `PUT /sessions/{id}/files` answers.
type WriteFileResponse struct {
	Path  string `json:"path"`
	Bytes uint64 `json:"bytes"`
}

// FetchedPathHeader is the response header naming where a fetched path
// resolved on the target. It is form-urlencoded, because a path need not be
// valid header text.
const FetchedPathHeader = "x-mj-fetched-path"

// DecodeFetchedPath is the target path a FetchedPathHeader value names, or
// false when it names none.
//
// The value is one form-urlencoded token with no `=` or `&` left in it, so
// it parses as a single key.
func DecodeFetchedPath(value []byte) (string, bool) {
	// url::form_urlencoded::parse: `&`-separated sequences, empty ones
	// skipped, each a name and an optional `=`-separated value.
	for _, sequence := range strings.Split(string(value), "&") {
		if sequence == "" {
			continue
		}
		name, _, _ := strings.Cut(sequence, "=")
		path := formURLDecode(name)
		return path, path != ""
	}
	return "", false
}

// formURLDecode is form_urlencoded's decoding of one name or value: `+` is a
// space, a `%` with two hex digits is that byte, anything else is itself,
// and the bytes are read as lossy UTF-8.
func formURLDecode(text string) string {
	decoded := make([]byte, 0, len(text))
	for index := 0; index < len(text); index++ {
		switch b := text[index]; {
		case b == '+':
			decoded = append(decoded, ' ')
		case b == '%' && index+2 < len(text) && isHexDigit(text[index+1]) && isHexDigit(text[index+2]):
			decoded = append(decoded, hexValue(text[index+1])<<4|hexValue(text[index+2]))
			index += 2
		default:
			decoded = append(decoded, b)
		}
	}
	return core.LossyUTF8(decoded)
}

func isHexDigit(b byte) bool {
	return '0' <= b && b <= '9' || 'a' <= b && b <= 'f' || 'A' <= b && b <= 'F'
}

func hexValue(b byte) byte {
	switch {
	case '0' <= b && b <= '9':
		return b - '0'
	case 'a' <= b && b <= 'f':
		return b - 'a' + 10
	}
	return b - 'A' + 10
}
