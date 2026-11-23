// Port of mj-harness-install/src/login_environment.rs (the Windows discover).

//go:build windows

package harnessinstall

import (
	"errors"
	"fmt"
	"log/slog"
	"strings"
	"unicode/utf16"
	"unsafe"

	"golang.org/x/sys/windows"
)

// discoverWindowsAccount asks Windows to build the account's registry-backed
// environment, including expanded values and the combined system/user PATH.
// Never inherit the caller's session overlay (bInherit = FALSE).
// https://learn.microsoft.com/windows/win32/api/userenv/nf-userenv-createenvironmentblock
func discoverWindowsAccount() (LoginEnvironmentEnvironment, error) {
	var token windows.Token
	if err := windows.OpenProcessToken(windows.CurrentProcess(), windows.TOKEN_QUERY|windows.TOKEN_DUPLICATE, &token); err != nil {
		return nil, fmt.Errorf("open account environment token: %w", err)
	}
	defer token.Close()
	var block *uint16
	if err := windows.CreateEnvironmentBlock(&block, token, false); err != nil {
		return nil, fmt.Errorf("build account environment: %w", err)
	}
	defer func() {
		if err := windows.DestroyEnvironmentBlock(block); err != nil {
			slog.Warn("release account environment", "error", err)
		}
	}()
	environment := LoginEnvironmentEnvironment{}
	// Windows returns a double-NUL terminated sequence of NUL terminated
	// UTF-16 entries. The allocation remains live through parsing.
	entry := block
	for *entry != 0 {
		length := 0
		for *(*uint16)(unsafe.Add(unsafe.Pointer(entry), length*2)) != 0 {
			length++
		}
		units := unsafe.Slice(entry, length)
		if !validUTF16(units) {
			return nil, errors.New("account environment is not valid UTF-16")
		}
		text := string(utf16.Decode(units))
		// Windows drive-current-directory entries start with '=' and
		// belong to the process, not the account environment.
		if name, value, ok := strings.Cut(text, "="); ok && name != "" {
			environment[name] = value
		}
		entry = (*uint16)(unsafe.Add(unsafe.Pointer(entry), (length+1)*2))
	}
	return environment, nil
}

// validUTF16 reports whether units has no unpaired surrogate, as Rust's
// String::from_utf16 requires.
func validUTF16(units []uint16) bool {
	for i := 0; i < len(units); i++ {
		unit := units[i]
		switch {
		case unit >= 0xD800 && unit < 0xDC00:
			if i+1 >= len(units) || units[i+1] < 0xDC00 || units[i+1] >= 0xE000 {
				return false
			}
			i++
		case unit >= 0xDC00 && unit < 0xE000:
			return false
		}
	}
	return true
}
