// Port of the macOS find_bundled_binary in mj-targets/src/tailscale.rs.

package targets

import "os"

func findBundledBinary() (string, bool) {
	const bundled = "/Applications/Tailscale.app/Contents/MacOS/Tailscale"
	info, err := os.Stat(bundled)
	if err != nil || !info.Mode().IsRegular() {
		return "", false
	}
	return bundled, true
}
