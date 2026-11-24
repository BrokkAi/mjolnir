// Port of mj-worker-staging/src/worker_binary/install.rs's `mbx_shim_tests`
// module.

package workerstaging

import (
	"bytes"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func mbxExecuteScript(t *testing.T, script string, arguments []string, path, home *string) core.CommandOutput {
	t.Helper()
	args := append([]string{"-c", script, "sh"}, arguments...)
	command := core.NewCommandSpec("sh", args)
	if command.Env == nil {
		command.Env = map[string]string{}
	}
	if path != nil {
		command.Env["PATH"] = *path + ":/usr/bin:/bin"
	}
	if home != nil {
		command.Env["HOME"] = *home
	}
	output, err := core.ProcessExecutor{}.Execute(&command)
	if err != nil {
		t.Fatal(err)
	}
	return output
}

func mbxRunScript(t *testing.T, script string, arguments []string) {
	t.Helper()
	output := mbxExecuteScript(t, script, arguments, nil, nil)
	if output.Status != 0 {
		t.Fatalf("status %d: %s", output.Status, lossyText(output.Stderr))
	}
}

func TestOnlyMarkedMbxShimsAreRemoved(t *testing.T) {
	if runtime.GOOS != "linux" {
		t.Skip("Linux only, as in Rust")
	}
	root := t.TempDir()
	generated := filepath.Join(root, "generated")
	mbxRunScript(t, installMbxShimsScript, []string{
		generated,
		"MBX_CARGO_SHIM_MODE=1\nMBX_CARGO_SHIM_PATH=$(command -v \"$0\")\nexport MBX_CARGO_SHIM_MODE MBX_CARGO_SHIM_PATH\nexec /cache/.mjolnir/bin/mbx \"$@\"",
		"exec /cache/.mjolnir/bin/mbx \"$@\"",
	})
	for _, name := range []string{"cargo", "mbx"} {
		script, err := os.ReadFile(filepath.Join(generated, name))
		if err != nil {
			t.Fatal(err)
		}
		if !slices.Contains(installLines(string(script)), "# mjolnir-mbx-shim") {
			t.Fatalf("%s has no shim marker: %q", name, script)
		}
	}

	legacy := filepath.Join(root, "legacy")
	if err := os.Mkdir(legacy, 0o777); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(legacy, "cargo"), []byte("legacy cargo binary"), 0o666); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(legacy, "mbx"), []byte("legacy copied mbx binary"), 0o666); err != nil {
		t.Fatal(err)
	}

	mbxRunScript(t, removeMbxShimsScript, []string{generated})
	mbxRunScript(t, removeMbxShimsScript, []string{legacy})

	for _, name := range []string{"cargo", "mbx"} {
		if _, err := os.Stat(filepath.Join(generated, name)); err == nil {
			t.Fatalf("generated %s survived", name)
		}
	}
	if got, _ := os.ReadFile(filepath.Join(legacy, "cargo")); string(got) != "legacy cargo binary" {
		t.Fatalf("legacy cargo: %q", got)
	}
	if got, _ := os.ReadFile(filepath.Join(legacy, "mbx")); string(got) != "legacy copied mbx binary" {
		t.Fatalf("legacy mbx: %q", got)
	}
}

func TestLegacyDetectionDistinguishesGrepMissMatchAndFailure(t *testing.T) {
	if runtime.GOOS != "linux" {
		t.Skip("Linux only, as in Rust")
	}
	temp := t.TempDir()
	source := filepath.Join(temp, "shared config")
	if err := os.MkdirAll(source, 0o777); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(source, "config.toml"), []byte("[cache]\n"), 0o666); err != nil {
		t.Fatal(err)
	}
	roots := filepath.Join(temp, "config roots")
	home := filepath.Join(temp, "home")
	legacy := filepath.Join(temp, "legacy")
	for _, directory := range []string{legacy, home} {
		if err := os.MkdirAll(directory, 0o777); err != nil {
			t.Fatal(err)
		}
	}
	if err := os.WriteFile(filepath.Join(legacy, "mbx"), []byte("private copied mbx binary"), 0o666); err != nil {
		t.Fatal(err)
	}
	args := []string{legacy, source, roots}

	legacyOutput := mbxExecuteScript(t, checkLegacyMbxAndLinkConfigScript, args, nil, &home)
	if legacyOutput.Status != 0 || string(legacyOutput.Stdout) != "legacy" {
		t.Fatalf("legacy: status %d stdout %q stderr %q", legacyOutput.Status, legacyOutput.Stdout, legacyOutput.Stderr)
	}

	if err := os.WriteFile(filepath.Join(legacy, "mbx"), []byte("#!/bin/sh\n# mjolnir-mbx-shim\n"), 0o666); err != nil {
		t.Fatal(err)
	}
	newOutput := mbxExecuteScript(t, checkLegacyMbxAndLinkConfigScript, args, nil, &home)
	if newOutput.Status != 0 || string(newOutput.Stdout) != "new" {
		t.Fatalf("new: status %d stdout %q stderr %q", newOutput.Status, newOutput.Stdout, newOutput.Stderr)
	}

	tools := filepath.Join(temp, "tools")
	if err := os.MkdirAll(tools, 0o777); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(tools, "grep"), []byte("#!/bin/sh\nexit 2\n"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(filepath.Join(tools, "grep"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(legacy, "mbx"), []byte("private copied mbx binary"), 0o666); err != nil {
		t.Fatal(err)
	}
	failed := mbxExecuteScript(t, checkLegacyMbxAndLinkConfigScript, args, &tools, &home)
	if failed.Status == 0 {
		t.Fatal("a failed grep passed the inspection")
	}
	if !bytes.Contains(failed.Stderr, []byte("grep exited 2")) {
		t.Fatalf("%s", failed.Stderr)
	}
}

func TestLegacyDetectionErrorsAreReturnedToTheCaller(t *testing.T) {
	failedInspection := &workerTestExecutor{
		execute: func(*core.CommandSpec) (core.CommandOutput, error) {
			return commandOutputOf(1, "", "could not inspect the Mjolnir mbx shim marker in /worker/bin/mbx (grep exited 2)"), nil
		},
	}
	_, err := LinkLegacyMbxConfiguration(failedInspection, &core.TargetsTargetLocator{
		Kind:        core.TargetsTargetLocatorLocalPodman,
		ContainerID: "mj-test-container",
		WorkspaceStorage: core.TargetsPodmanWorkspaceLocator{
			Kind: core.TargetsPodmanWorkspaceLocatorContainerLayer,
		},
	}, "/worker", "/cache/.mjolnir/config/mbx", nil)
	if err == nil {
		t.Fatal("a failed inspection succeeded")
	}
	if message := err.Error(); !strings.Contains(message, "grep exited 2") {
		t.Fatal(message)
	}
}
