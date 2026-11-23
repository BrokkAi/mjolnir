// Port of mj-chat/src/chat/pasted_paths.rs (not Linux).

//go:build !linux

package chat

import "fmt"

// windowsPathOnThisHost: only WSL can open a pasted Windows path, so off
// Linux it cannot be translated.
func windowsPathOnThisHost(path string) (string, error) {
	return "", fmt.Errorf("%s is a Windows path, which this machine cannot open", path)
}
