// Port of the tests in mj-targets/src/targets/platform.rs.

package targets

import "testing"

func TestDetectsSupportedPlatformsAndRejectsUnknownResponses(t *testing.T) {
	for _, supported := range []struct {
		text         string
		os           TargetOs
		architecture string
	}{
		{"Darwin arm64\n", TargetOsDarwin, "aarch64"},
		{"Darwin x86_64\n", TargetOsDarwin, "x86_64"},
		{"Linux aarch64\n", TargetOsLinux, "aarch64"},
		{"Linux x86_64\n", TargetOsLinux, "x86_64"},
	} {
		platform, err := TargetPlatformParse(supported.text)
		if err != nil {
			t.Fatalf("%q: %v", supported.text, err)
		}
		if want := (TargetPlatform{Os: supported.os, Architecture: supported.architecture}); platform != want {
			t.Fatalf("%q = %+v, want %+v", supported.text, platform, want)
		}
	}
	for _, text := range []string{"FreeBSD arm64", "Linux riscv64", "Darwin", "", "Linux x86_64 junk"} {
		if _, err := TargetPlatformParse(text); err == nil {
			t.Fatalf("%q was accepted", text)
		}
	}
}
