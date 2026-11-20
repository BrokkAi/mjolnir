//go:build !windows

package rspath

// Host is the flavour of the platform the program is built for.
const Host = Unix

// MainSeparator is std::path::MAIN_SEPARATOR.
const MainSeparator = '/'

// MainSeparatorStr is std::path::MAIN_SEPARATOR_STR.
const MainSeparatorStr = "/"
