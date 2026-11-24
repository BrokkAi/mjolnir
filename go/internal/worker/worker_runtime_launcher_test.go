//go:build unix

package worker

import (
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

// Hard-won: #1589: rotating Git setup hid the stable cargo and mbx launchers.
func TestMbxLaunchersSurviveShellResetAndGitReload(t *testing.T) {
	root := t.TempDir()
	bin := filepath.Join(root, "bin")
	if err := os.Mkdir(bin, 0755); err != nil {
		t.Fatal(err)
	}
	for _, tool := range []string{"cargo", "mbx", "gh"} {
		if err := os.WriteFile(filepath.Join(bin, tool), []byte("#!/bin/sh\nexit 0\n"), 0755); err != nil {
			t.Fatal(err)
		}
	}
	miseShims := filepath.Join(root, "mise", "shims")
	if err := os.MkdirAll(miseShims, 0755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(miseShims, "cargo"), []byte("#!/bin/sh\nexit 99\n"), 0755); err != nil {
		t.Fatal(err)
	}
	bare := map[string]string{}
	if err := restoreMbxLauncher(root, nil, bare); err != nil {
		t.Fatal(err)
	}
	if _, ok := bare["MJ_MBX_BIN"]; ok {
		t.Fatal("injected bare launcher")
	}
	env := map[string]string{"PATH": "/opt/mise/shims:/usr/bin:/bin", "MBX_SHIMS_DIR": filepath.Join(root, "mbx-shims"), "MISE_DATA_DIR": filepath.Join(root, "mise")}
	check := func(reset bool) {
		t.Helper()
		script := "command -v cargo; command -v mbx"
		if reset {
			script = "command -v cargo; command -v mbx"
		}
		cmd := exec.Command("/bin/bash", "-c", script)
		for k, v := range env {
			if reset && k == "PATH" {
				v = "/opt/mise/shims:/usr/bin:/bin"
			}
			cmd.Env = append(cmd.Env, k+"="+v)
		}
		out, err := cmd.CombinedOutput()
		if err != nil {
			t.Fatalf("%v: %s", err, out)
		}
		want := bin + "/cargo\n" + bin + "/mbx\n"
		if string(out) != want {
			t.Fatalf("got %q want %q", out, want)
		}
	}
	for _, present := range []bool{false, true} {
		env := map[string]string{"MJ_MBX_BIN": bin}
		if present {
			env["PATH"] = ""
		}
		if err := restoreMbxLauncher(root, nil, env); err != nil {
			t.Fatal(err)
		}
		if env["PATH"] != bin {
			t.Fatalf("restored empty PATH: %q", env["PATH"])
		}
		delete(env, "PATH")
		if present {
			env["PATH"] = ""
		}
		// sh may supply a default PATH when absent, so explicitly unset it in that case.
		script := launcherPathShell() + `printf '%s' "$PATH"`
		if !present {
			script = "unset PATH\n" + script
		}
		cmd := exec.Command("/bin/sh", "-c", script)
		for k, v := range env {
			cmd.Env = append(cmd.Env, k+"="+v)
		}
		out, err := cmd.Output()
		if err != nil || string(out) != bin {
			t.Fatalf("shell empty PATH: %q %v", out, err)
		}
	}
	relative := ".local/share/mjolnir/workers/sess/bin"
	relativeHome := "relative-home"
	for _, home := range []*string{&root, nil, &relativeHome} {
		env := map[string]string{"MJ_MBX_BIN": relative}
		if err := restoreMbxLauncher(root, home, env); err != nil {
			t.Fatal(err)
		}
		expected := bin
		if home == &root {
			expected = filepath.Join(root, relative)
		}
		if env["MJ_MBX_BIN"] != expected || env["PATH"] != expected {
			t.Fatalf("relative launcher: %#v", env)
		}
	}
	for _, generation := range []string{"harness-git-first", "harness-git-reload"} {
		gitBin := filepath.Join(root, generation, "bin")
		if err := os.MkdirAll(gitBin, 0755); err != nil {
			t.Fatal(err)
		}
		gitGh := filepath.Join(gitBin, "gh")
		if err := os.WriteFile(gitGh, []byte("#!/bin/sh\nexit 0\n"), 0755); err != nil {
			t.Fatal(err)
		}
		env["MJ_GITHUB_CLI_BIN"] = gitBin
		if err := restoreMbxLauncher(root, nil, env); err != nil {
			t.Fatal(err)
		}
		check(false)
		if err := projectToolsApplyEnvironment(env, []string{root}, nil); err != nil {
			t.Fatal(err)
		}
		check(false)
		if err := shellEnvironmentConfigure(filepath.Join(root, "bash-env"), env); err != nil {
			t.Fatal(err)
		}
		check(true)
		cmd := exec.Command("/bin/bash", "-c", `. "$MJ_SESSION_BASH_ENV"; . "$MJ_SESSION_BASH_ENV"; command -v gh; command -v cargo; printf '%s' "$PATH"`)
		for k, v := range env {
			if k == "PATH" {
				v = "/opt/mise/shims:" + bin + ":/usr/bin:/bin:" + bin
			}
			cmd.Env = append(cmd.Env, k+"="+v)
		}
		out, err := cmd.CombinedOutput()
		if err != nil {
			t.Fatalf("%v: %s", err, out)
		}
		parts := strings.SplitN(string(out), "\n", 3)
		if len(parts) != 3 || parts[0] != gitGh || parts[1] != filepath.Join(bin, "cargo") {
			t.Fatalf("resolved: %q", out)
		}
		entries := filepath.SplitList(parts[2])
		count := 0
		for _, entry := range entries {
			if entry == bin {
				count++
			}
		}
		if len(entries) < 2 || entries[0] != gitBin || entries[1] != bin || count != 1 {
			t.Fatalf("PATH: %s", parts[2])
		}
	}
	cmd := exec.Command("/bin/sh", "-c", bazelPathScript()+"printf '%s' \"$PATH\"")
	cmd.Env = []string{"PATH=" + bin + ":/usr/bin", "MJ_MBX_BIN=" + bin}
	out, err := cmd.Output()
	if err != nil || strings.TrimSpace(string(out)) != "/usr/bin" {
		t.Fatalf("bazel PATH: %s %v", out, err)
	}
}
