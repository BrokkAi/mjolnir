//go:build unix

// Port of the test-only helper in mj-accounts/src/codex_usage.rs.

package accounts

import (
	"encoding/json"
	"os"
	"path/filepath"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// installFakeManagedCodex installs script as the managed Codex CLI under
// cacheRoot, with its lease and manifest, and returns the installation
// directory. The quota tests share it.
func installFakeManagedCodex(cacheRoot, script string) string {
	install := core.ManagedHarnessInstallDir(cacheRoot, core.HarnessKindCodex)
	launcher := filepath.Join(install, filepath.FromSlash(core.CodexCLIEntrypoint))
	launcherDirectory := filepath.Dir(launcher)
	if err := os.MkdirAll(launcherDirectory, 0o755); err != nil {
		panic("create managed install: " + err.Error())
	}
	core.InstallFakeCommand(launcherDirectory, "codex", script)
	if err := os.WriteFile(filepath.Join(install, core.LeaseFile), nil, 0o644); err != nil {
		panic("create managed install lease: " + err.Error())
	}
	manifest, err := json.Marshal(core.ManagedHarnessManifestForHarness(core.HarnessKindCodex))
	if err != nil {
		panic("serialize managed install manifest: " + err.Error())
	}
	if err := os.WriteFile(filepath.Join(install, core.ManifestFile), manifest, 0o644); err != nil {
		panic("write managed install manifest: " + err.Error())
	}
	return install
}
