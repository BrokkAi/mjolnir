// Port of mj-targets/src/targets/platform.rs.

package targets

import (
	"fmt"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

type TargetOs int

const (
	TargetOsLinux TargetOs = iota
	TargetOsDarwin
)

// TargetPlatform is the platform of the execution boundary, which may differ
// from both the controller and the host of a Linux container.
type TargetPlatform struct {
	Os           TargetOs
	Architecture string
}

// TargetPlatformParse reads `uname -sm` output (Rust: TargetPlatform::parse).
func TargetPlatformParse(uname string) (TargetPlatform, error) {
	panic("unported: mj-targets/src/targets/platform.rs TargetPlatform::parse")
}

func NormalizeArchitecture(architecture string) (string, error) {
	switch architecture {
	case "x86_64", "amd64":
		return "x86_64", nil
	case "aarch64", "arm64":
		return "aarch64", nil
	default:
		return "", fmt.Errorf("unsupported target architecture %s", core.RustDebugString(architecture))
	}
}

func PlatformProbe(locator *core.TargetsTargetLocator) core.CommandSpec {
	return core.LocatorCommand(core.SshTrafficClassControl, locator, []string{"uname", "-sm"}).
		WithPurpose("detect target platform")
}

// SessionImagePlatform is the OCI platform of a container target, before its
// container exists.
func SessionImagePlatform(template core.TargetTemplate, executor core.CommandExecutor) (string, error) {
	panic("unported: mj-targets/src/targets/platform.rs session_image_platform")
}
