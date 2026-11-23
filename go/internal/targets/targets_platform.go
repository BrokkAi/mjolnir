// Port of mj-targets/src/targets/platform.rs.

package targets

import (
	"errors"
	"fmt"
	"strings"

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
	fields := strings.Fields(uname)
	field := func(index int) (string, bool) {
		if index < len(fields) {
			return fields[index], true
		}
		return "", false
	}
	var os TargetOs
	switch first, ok := field(0); {
	case ok && first == "Linux":
		os = TargetOsLinux
	case ok && first == "Darwin":
		os = TargetOsDarwin
	case ok:
		return TargetPlatform{}, fmt.Errorf("unsupported target operating system Some(%s)", core.RustDebugString(first))
	default:
		return TargetPlatform{}, errors.New("unsupported target operating system None")
	}
	second, _ := field(1)
	architecture, err := NormalizeArchitecture(second)
	if err != nil {
		return TargetPlatform{}, err
	}
	if len(fields) > 2 {
		return TargetPlatform{}, fmt.Errorf("invalid target platform response %s", core.RustDebugString(uname))
	}
	return TargetPlatform{Os: os, Architecture: architecture}, nil
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
	host, container, ok := core.TargetTemplateImageHost(template)
	if !ok {
		return "", errors.New("image operation requires a container target")
	}
	var architecture string
	switch {
	case container.Platform != nil:
		for _, part := range strings.Split(*container.Platform, "/") {
			if normalized, err := NormalizeArchitecture(part); err == nil {
				architecture = normalized
				break
			}
		}
		if architecture == "" {
			return "", errors.New("unsupported container platform")
		}
	case host.Kind == core.ImageHostAppleContainer || host.Kind == core.ImageHostSshAppleContainer:
		architecture = "aarch64"
	default:
		// The engine owns its native architecture, including Docker Desktop
		// on a Windows daemon host where uname is not available.
		var args []string
		switch host.Engine() {
		case "podman":
			args = []string{"podman", "info", "--format", "{{.Host.Arch}}"}
		case "docker":
			args = []string{"docker", "info", "--format", "{{.Architecture}}"}
		default:
			args = []string{"uname", "-sm"} // Nitro hosts run Linux.
		}
		output, err := ExecuteChecked(executor, host.Command(args, "detect image platform"))
		if err != nil {
			return "", err
		}
		text, err := libFromUTF8(output.Stdout)
		if err != nil {
			return "", err
		}
		if host.Kind == core.ImageHostSshNitro {
			platform, err := TargetPlatformParse(text)
			if err != nil {
				return "", err
			}
			architecture = platform.Architecture
		} else if architecture, err = NormalizeArchitecture(strings.TrimSpace(text)); err != nil {
			return "", err
		}
	}
	switch architecture {
	case "x86_64":
		return "linux/amd64", nil
	case "aarch64":
		return "linux/arm64", nil
	}
	panic("unreachable: NormalizeArchitecture names only x86_64 and aarch64")
}
