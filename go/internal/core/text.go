// Port of mj-core/src/text.rs.

// Small helpers for the words Mjolnir shows people.

package core

import "fmt"

// countedNumber is the integer types Counted accepts.
type countedNumber interface {
	~int | ~int8 | ~int16 | ~int32 | ~int64 |
		~uint | ~uint8 | ~uint16 | ~uint32 | ~uint64 | ~uintptr
}

// Counted is count and the noun that agrees with it: `1 command`,
// `2 commands`, `0 entries`. Counts shown to a person go through this, so
// none reads "1 commands" or "2 session(s)" (launch findings C-12 and R5-10).
func Counted[T countedNumber](count T, singular, plural string) string {
	noun := plural
	if count == 1 {
		noun = singular
	}
	return fmt.Sprintf("%d %s", count, noun)
}

// FormatResourceBytes formats a byte count as `512B`, `1.5K`, `3.2M`,
// `14.2G` or `1.0T`.
func FormatResourceBytes(bytes uint64) string {
	const (
		kib = 1024.0
		mib = kib * 1024.0
		gib = mib * 1024.0
		tib = gib * 1024.0
	)
	switch {
	case bytes < 1024:
		return fmt.Sprintf("%dB", bytes)
	case bytes < 1024*1024:
		return fmt.Sprintf("%.1fK", float64(bytes)/kib)
	case bytes < 1024*1024*1024:
		return fmt.Sprintf("%.1fM", float64(bytes)/mib)
	case bytes < 1024*1024*1024*1024:
		return fmt.Sprintf("%.1fG", float64(bytes)/gib)
	default:
		return fmt.Sprintf("%.1fT", float64(bytes)/tib)
	}
}

// RefreshAge says how long ago refreshed was, as `12s ago`, `5m ago` or
// `3h ago`; a zero timestamp means the time is not known.
func RefreshAge(now, refreshed uint64) string {
	if refreshed == 0 {
		return "unknown"
	}
	age := uint64(0)
	if now > refreshed {
		age = now - refreshed
	}
	switch {
	case age < 60:
		return fmt.Sprintf("%ds ago", age)
	case age < 3_600:
		return fmt.Sprintf("%dm ago", age/60)
	default:
		return fmt.Sprintf("%dh ago", age/3_600)
	}
}
