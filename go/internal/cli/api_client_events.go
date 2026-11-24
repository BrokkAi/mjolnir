// Port of mj-cli/src/api_client/events.rs.
//
// Bounded SSE decoding, independent of network chunk boundaries.

package cli

import (
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/server"
)

// eventFrameLimit is the most bytes one SSE frame may carry (8 MiB).
const eventFrameLimit = 8 * 1024 * 1024

// eventDecoder decodes the `GET /events` stream into API events (Rust
// `EventDecoder`). The zero value is ready to use.
type eventDecoder struct {
	line       []byte
	data       []byte
	id         *uint64
	kind       string
	frameBytes int
	// handoffCursor is set once the daemon announced that an upgrade is
	// replacing it.
	handoffCursor *uint64
}

// handoff is the cursor to resume from once the daemon announced that an
// upgrade is replacing it. Nothing after that announcement is decoded.
func (d *eventDecoder) handoff() (uint64, bool) {
	if d.handoffCursor == nil {
		return 0, false
	}
	return *d.handoffCursor, true
}

// push decodes bytes, returning the events whose frames they completed.
func (d *eventDecoder) push(bytes []byte) ([]core.ApiEvent, error) {
	var events []core.ApiEvent
	if d.handoffCursor != nil {
		return events, nil
	}
	for _, b := range bytes {
		d.frameBytes++
		if d.frameBytes > eventFrameLimit {
			return nil, errors.New("API event exceeds 8 MiB")
		}
		if b != '\n' {
			d.line = append(d.line, b)
			continue
		}
		raw := d.line
		d.line = nil
		line, err := cliFromUTF8(raw)
		if err != nil {
			return nil, err
		}
		line = strings.TrimRight(line, "\r")
		if line == "" {
			if d.kind == "stream_error" {
				return nil, errors.New(strings.TrimRightFunc(string(d.data), isRustWhitespace))
			}
			if d.kind == server.DaemonHandoffCode {
				if d.id == nil {
					return nil, errors.New("the daemon handoff announcement names no event ID")
				}
				cursor := *d.id
				d.handoffCursor = &cursor
				return events, nil
			}
			if len(d.data) > 0 {
				var event core.ApiEvent
				if err := json.Unmarshal(d.data, &event); err != nil {
					return nil, err
				}
				_, activity := event.Event.(core.ApiEventDataActivityChanged)
				if !((d.id != nil && *d.id == event.Seq) || (d.id == nil && activity)) {
					return nil, errors.New("SSE ID does not match API event sequence")
				}
				if d.kind != event.Event.Kind() {
					return nil, errors.New("SSE type does not match API event type")
				}
				events = append(events, event)
			}
			d.data = d.data[:0]
			d.id = nil
			d.kind = ""
			d.frameBytes = 0
		} else if !strings.HasPrefix(line, ":") {
			field, value, found := strings.Cut(line, ":")
			if !found {
				field, value = line, ""
			}
			value = strings.TrimPrefix(value, " ")
			switch field {
			case "data":
				d.data = append(d.data, value...)
				d.data = append(d.data, '\n')
			case "id":
				id, err := parseRustU64(value)
				if err != nil {
					return nil, err
				}
				d.id = &id
			case "event":
				d.kind = value
			}
		}
	}
	return events, nil
}

// isRustWhitespace is char::is_whitespace, which str::trim_end uses.
func isRustWhitespace(r rune) bool {
	switch {
	case r == ' ' || (r >= '\t' && r <= '\r'):
		return true
	case r < 0x80:
		return false
	}
	switch r {
	case 0x85, 0xA0, 0x1680, 0x2028, 0x2029, 0x202F, 0x205F, 0x3000:
		return true
	}
	return r >= 0x2000 && r <= 0x200A
}

// parseRustU64 is `str::parse::<u64>`, failing with the text of Rust's
// ParseIntError. One leading `+` is accepted.
func parseRustU64(text string) (uint64, error) {
	digits := strings.TrimPrefix(text, "+")
	if text == "" {
		return 0, errors.New("cannot parse integer from empty string")
	}
	if digits == "" {
		return 0, errors.New("invalid digit found in string")
	}
	var value uint64
	for index := 0; index < len(digits); index++ {
		digit := digits[index]
		if digit < '0' || digit > '9' {
			return 0, errors.New("invalid digit found in string")
		}
		if value > (math.MaxUint64-uint64(digit-'0'))/10 {
			return 0, errors.New("number too large to fit in target type")
		}
		value = value*10 + uint64(digit-'0')
	}
	return value, nil
}

// cliFromUTF8 is Rust's `std::str::from_utf8`, failing with the text of
// Rust's Utf8Error ("invalid utf-8 sequence of 1 bytes from index 3",
// "incomplete utf-8 byte sequence from index 3").
func cliFromUTF8(text []byte) (string, error) {
	index := 0
	for index < len(text) {
		first := text[index]
		if first < 0x80 {
			index++
			continue
		}
		width := 0
		switch {
		case first >= 0xC2 && first <= 0xDF:
			width = 2
		case first >= 0xE0 && first <= 0xEF:
			width = 3
		case first >= 0xF0 && first <= 0xF4:
			width = 4
		}
		if width == 0 {
			return "", fmt.Errorf("invalid utf-8 sequence of 1 bytes from index %d", index)
		}
		low, high := byte(0x80), byte(0xBF)
		switch first {
		case 0xE0:
			low = 0xA0
		case 0xED:
			high = 0x9F
		case 0xF0:
			low = 0x90
		case 0xF4:
			high = 0x8F
		}
		for offset := 1; offset < width; offset++ {
			if index+offset >= len(text) {
				return "", fmt.Errorf("incomplete utf-8 byte sequence from index %d", index)
			}
			next := text[index+offset]
			if offset == 1 && (next < low || next > high) || offset > 1 && (next < 0x80 || next > 0xBF) {
				return "", fmt.Errorf("invalid utf-8 sequence of %d bytes from index %d", offset, index)
			}
		}
		index += width
	}
	return string(text), nil
}
