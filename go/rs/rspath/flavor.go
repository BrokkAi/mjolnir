package rspath

// Flavor says which platform's path syntax a function follows.
//
// Rust compiles one syntax into std::path: `/` separators and no prefixes on
// Unix, or `\` and `/` separators with drive, UNC and verbatim prefixes on
// Windows. This package holds both, so that either can be used and tested on
// any host. The package-level functions (Join, Parent, Components, ...) are
// the methods of Host, the flavour of the platform the program is built for,
// which is what the same call does in the Rust program. Unix and Windows are
// for code that must follow one syntax wherever it runs:
//
//	rspath.Join(base, name)            // as std::path does on this platform
//	rspath.Unix.Join(base, name)       // always as on Unix
//	rspath.Windows.IsAbsolute(path)    // always as on Windows
//
// The zero Flavor is Host.
type Flavor uint8

const (
	// flavorHost is the zero value, and stands for Host.
	flavorHost Flavor = iota
	// Unix is std::path as compiled for a Unix target (Linux, macOS).
	Unix
	// Windows is std::path as compiled for Windows.
	Windows
)

// resolve replaces the zero value by the host's flavour.
func (f Flavor) resolve() Flavor {
	if f == flavorHost {
		return Host
	}
	return f
}

// hasPrefixes is sys::path::HAS_PREFIXES.
func (f Flavor) hasPrefixes() bool {
	return f.resolve() == Windows
}

// String names the flavour, for test output.
func (f Flavor) String() string {
	if f.resolve() == Windows {
		return "Windows"
	}
	return "Unix"
}

// MainSeparator is std::path::MAIN_SEPARATOR for the flavour: `/` on Unix
// and `\` on Windows.
func (f Flavor) MainSeparator() rune {
	if f.resolve() == Windows {
		return '\\'
	}
	return '/'
}

// MainSeparatorStr is std::path::MAIN_SEPARATOR_STR for the flavour.
func (f Flavor) MainSeparatorStr() string {
	if f.resolve() == Windows {
		return `\`
	}
	return "/"
}

// Separators is std::path::SEPARATORS for the flavour: every character that
// separates components, the main one first.
func (f Flavor) Separators() []rune {
	if f.resolve() == Windows {
		return []rune{'\\', '/'}
	}
	return []rune{'/'}
}

// SeparatorsStr is std::path::SEPARATORS_STR for the flavour.
func (f Flavor) SeparatorsStr() []string {
	if f.resolve() == Windows {
		return []string{`\`, "/"}
	}
	return []string{"/"}
}

// IsSeparator is std::path::is_separator for the flavour.
func (f Flavor) IsSeparator(c rune) bool {
	return c >= 0 && c < 0x80 && f.isSepByte(byte(c))
}

// IsSeparator is std::path::is_separator: whether c separates path
// components on this platform.
func IsSeparator(c rune) bool { return Host.IsSeparator(c) }

// Separators is std::path::SEPARATORS.
func Separators() []rune { return Host.Separators() }

// SeparatorsStr is std::path::SEPARATORS_STR.
func SeparatorsStr() []string { return Host.SeparatorsStr() }
