// Port of the non-macOS find_bundled_binary in mj-targets/src/tailscale.rs.

//go:build !darwin

package targets

func findBundledBinary() (string, bool) {
	return "", false
}
