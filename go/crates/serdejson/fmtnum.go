package serdejson

import "strconv"

// formatF64 is `zmij::Buffer::new().format_finite(value)`: how serde_json
// writes a finite f64.
//
// PLACEHOLDER until the port of zmij's layout lands in this file.
func formatF64(value float64) string {
	return strconv.FormatFloat(value, 'g', -1, 64)
}
