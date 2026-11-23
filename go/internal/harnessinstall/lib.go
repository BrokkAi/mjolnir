// Port of mj-harness-install/src/lib.rs.

// Package harnessinstall is managed harness installation and target
// environment discovery for Mjolnir.
//
// The Rust crate's modules (harness_install, login_environment,
// program_path, session_image) all live in this one package, one file per
// Rust module.
package harnessinstall

import (
	"fmt"
	"path/filepath"
	"strings"
	"unicode"
)

// debugString formats a string as Rust's `{:?}` does.
func debugString(value string) string {
	var builder strings.Builder
	builder.WriteByte('"')
	for _, r := range value {
		switch r {
		case '"':
			builder.WriteString(`\"`)
		case '\\':
			builder.WriteString(`\\`)
		case '\n':
			builder.WriteString(`\n`)
		case '\r':
			builder.WriteString(`\r`)
		case '\t':
			builder.WriteString(`\t`)
		case 0:
			builder.WriteString(`\0`)
		default:
			if unicode.IsPrint(r) || r == ' ' {
				builder.WriteRune(r)
			} else {
				fmt.Fprintf(&builder, `\u{%x}`, r)
			}
		}
	}
	builder.WriteByte('"')
	return builder.String()
}

// canonicalizePath is Rust's `fs::canonicalize`: an absolute path with every
// link resolved, which must exist.
func canonicalizePath(path string) (string, error) {
	absolute, err := filepath.Abs(path)
	if err != nil {
		return "", err
	}
	return filepath.EvalSymlinks(absolute)
}

// isASCIIAlphanumeric is Rust's `u8::is_ascii_alphanumeric`.
func isASCIIAlphanumeric(c byte) bool {
	return ('0' <= c && c <= '9') || ('a' <= c && c <= 'z') || ('A' <= c && c <= 'Z')
}
