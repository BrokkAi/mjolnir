// Port of mj-worker/src/worker_runtime/tool_cache/tests.rs.

//go:build unix

package worker

import (
	"context"
	"errors"
	"fmt"
	"maps"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/harnessinstall"
)

func toolCacheTestEnvironment(root, checkout string) map[string]string {
	env := map[string]string{}
	for _, name := range []string{"PATH", "JAVA_HOME", "LANG", "SSL_CERT_FILE", "SSL_CERT_DIR"} {
		if value, ok := os.LookupEnv(name); ok {
			env[name] = value
		}
	}
	for _, pair := range [][2]string{
		{"HOME", filepath.Join(root, "home")},
		{"MJ_TOOL_CACHE_DIR", filepath.Join(root, "shared cache $literal")},
		{"MJ_TOOL_CACHE_HOME", filepath.Join(root, "workers", filepath.Base(checkout))},
		{"MJ_NX_HOME_DIR", filepath.Join(root, "home/.nx")},
		{"XDG_CONFIG_HOME", filepath.Join(root, "home/.config")},
		{"MBX_CACHE_DIR", filepath.Join(root, "mbx")},
		{"MBX_SHIMS_DIR", filepath.Join(root, "shims", filepath.Base(checkout))},
	} {
		env[pair[0]] = pair[1]
	}
	env["MJ_TOOL_CACHE_PROJECT"] = "cache-fixture.example/project"
	env["NX_DAEMON"] = "false"
	env["NX_NO_CLOUD"] = "true"
	env["DO_NOT_TRACK"] = "1"
	env["TURBO_TELEMETRY_DISABLED"] = "1"
	env["MBX_SUMMARY"] = "full"
	env["MBX_SAVINGS"] = "off"
	env["GOTOOLCHAIN"] = "local"
	// Both checkouts represent one revision. Separate container invocations
	// can cross a second boundary; Git's default clock would change Go's VCS
	// build metadata and correctly force a rebuild.
	for _, key := range []string{"GIT_AUTHOR_DATE", "GIT_COMMITTER_DATE"} {
		env[key] = "2026-01-01T00:00:00Z"
	}
	return env
}

var toolCacheTestNextContainer atomic.Uint64

func toolCacheTestRun(cwd string, env map[string]string, command string, args ...string) (string, error) {
	// Resolve like a shell; Command's own PATH search can otherwise consult
	// the test process PATH instead of the child's explicit environment.
	executable, found, err := harnessinstall.FindCommand(command, env)
	if err != nil {
		return "", err
	}
	if !found {
		return "", fmt.Errorf("test tool %s is missing", command)
	}
	child := exec.Command(executable, args...)
	child.Dir = cwd
	child.Env = workerRuntimeEnvironmentList(env)
	var container *string
	if image, ok := os.LookupEnv("MJ_CACHE_TEST_PODMAN_IMAGE"); ok {
		name := fmt.Sprintf("mj-cache-test-%d-%d", os.Getpid(), toolCacheTestNextContainer.Add(1)-1)
		root := filepath.Dir(env["HOME"])
		modules := os.Getenv("MJ_CACHE_TEST_NODE_MODULES")
		if modules == "" {
			return "", errors.New("environment variable not found")
		}
		tools := filepath.Dir(modules)
		podmanArgs := []string{
			"run", "--rm", "--pull=never", "--timeout=175", "--user=0:0",
			"--label=dev.mj.instance=tool-cache-test", "--name", name,
		}
		for _, mount := range [][2]string{{root, "rw"}, {tools, "ro"}} {
			podmanArgs = append(podmanArgs, "--volume", fmt.Sprintf("%s:%s:%s", mount[0], mount[0], mount[1]))
		}
		podmanArgs = append(podmanArgs, "--workdir", cwd)
		for _, name := range slices.Sorted(maps.Keys(env)) {
			podmanArgs = append(podmanArgs, "--env", name+"="+env[name])
		}
		podmanArgs = append(podmanArgs, image, executable)
		podmanArgs = append(podmanArgs, args...)
		child = exec.Command("podman", podmanArgs...)
		container = &name
	}
	output, runErr := core.RunBounded(context.Background(), child, 4*1024*1024, 180*time.Second)
	if container != nil {
		// Stop the container before any error can drop the fixture directory.
		cleaned, err := core.RunBounded(context.Background(), exec.Command("podman", "rm", "--ignore", "--force", *container), 64*1024, 30*time.Second)
		if err != nil {
			return "", err
		}
		if !cleaned.Status.Success() {
			return "", fmt.Errorf("could not stop fixture container %s", *container)
		}
	}
	if runErr != nil {
		return "", runErr
	}
	text := fromUTF8Lossy(output.Stdout) + fromUTF8Lossy(output.Stderr)
	if !output.Status.Success() {
		return "", fmt.Errorf("%s %q failed in %s:\n%s", command, args, cwd, text)
	}
	return text, nil
}

func toolCacheMust(t *testing.T, err error) {
	t.Helper()
	if err != nil {
		t.Fatal(err)
	}
}

// Hard-won: 1300d5692: a fork inherited the atomic launcher's writer and caused ETXTBSY.
func TestGeneratedLaunchersPreserveShellArgumentsAndExplicitBazelFlags(t *testing.T) {
	root := t.TempDir()
	checkout := filepath.Join(root, "checkout ' $name")
	tools := filepath.Join(root, "original tools")
	toolCacheMust(t, os.MkdirAll(checkout, 0o777))
	toolCacheMust(t, os.MkdirAll(tools, 0o777))
	toolCacheMust(t, os.WriteFile(filepath.Join(checkout, "MODULE.bazel"), nil, 0o666))
	// Exercise the writer's stdin beyond a pipe buffer and Linux's
	// per-argument limit, including shell quoting, without changing the
	// fixture's output.
	body := fmt.Sprintf("# %s\nprintf '<%%s>\\n' \"$@\"\n", strings.Repeat("quoted ' $literal ", 16*1024))
	toolCacheMust(t, writeLauncher(filepath.Join(tools, "bazel"), body))
	env := toolCacheTestEnvironment(root, checkout)
	env["PATH"] = tools
	toolCacheMust(t, toolCachePrepare(checkout, env))
	// Re-preparation must not resolve its own launcher and recurse forever.
	toolCacheMust(t, toolCachePrepare(checkout, env))
	output, err := toolCacheTestRun(checkout, env, "bazel",
		"--batch", "build", "--disk_cache=/explicit user's cache", "//:target $literal")
	toolCacheMust(t, err)
	lines := toolCacheLines(output)
	if !strings.HasPrefix(lines[0], "<--bazelrc=") {
		t.Fatalf("condition failed: %q", lines)
	}
	if !slices.Equal(lines[1:], []string{"<--batch>", "<build>", "<--disk_cache=/explicit user's cache>", "<//:target $literal>"}) {
		t.Fatalf("condition failed: %q", lines)
	}
	version, err := toolCacheTestRun(checkout, env, "bazel", "--version")
	toolCacheMust(t, err)
	if version != "<--version>\n" {
		t.Fatalf("condition failed: %q", version)
	}
	// Git setup and tool setup must replace the one owned shell hook, never
	// source each other in a cycle when the worker is prepared again.
	userHook := filepath.Join(root, "user bash env")
	toolCacheMust(t, os.WriteFile(userHook, []byte("if [ -n \"${USER_HOOK_SEEN:-}\" ]; then exit 99; fi\nexport USER_HOOK_SEEN=yes\n"), 0o666))
	env["BASH_ENV"] = userHook
	gitHook := filepath.Join(root, "github-shell-env")
	toolCacheMust(t, shellEnvironmentConfigure(gitHook, env))
	toolCacheMust(t, toolCachePrepare(checkout, env))
	toolCacheMust(t, shellEnvironmentConfigure(gitHook, env))
	toolCacheMust(t, toolCachePrepare(checkout, env))
	env["PATH"] = "/usr/bin:/bin"
	output, err = toolCacheTestRun(checkout, env, "bash", "-c", "printf '%s\\n' \"$USER_HOOK_SEEN\"; bazel --version")
	toolCacheMust(t, err)
	if output != "yes\n<--version>\n" {
		t.Fatal(output)
	}
	// Reviewers can prepare another checkout from the primary environment.
	// Its launcher must resolve the actual tool, not wrap the first checkout.
	other := filepath.Join(root, "review checkout")
	toolCacheMust(t, os.MkdirAll(other, 0o777))
	toolCacheMust(t, os.WriteFile(filepath.Join(other, "MODULE.bazel"), nil, 0o666))
	env["PATH"] = fmt.Sprintf("%s:%s", env["MJ_BUILD_TOOL_BIN"], tools)
	toolCacheMust(t, toolCachePrepare(other, env))
	output, err = toolCacheTestRun(other, env, "bazel", "build", "//:target")
	toolCacheMust(t, err)
	if strings.Count(output, "<--bazelrc=") != 1 {
		t.Fatal(output)
	}
}

// TestRealToolsReuseInvalidateAndBuildConcurrently is integration with
// actual tool formats and cache keys. The same portable test executable runs
// on a raw host and in Podman. Build artifacts stay in the explicitly
// supplied local-disk root, never in a temporary build directory.
func TestRealToolsReuseInvalidateAndBuildConcurrently(t *testing.T) {
	// Rust: #[ignore]; run it by setting MJ_CACHE_TEST_ROOT.
	if os.Getenv("MJ_CACHE_TEST_ROOT") == "" {
		t.Skip("requires Go, Gradle/JDK, Nx 23.2+, Turbo, Bazelisk, CMake, make, mbx and MJ_CACHE_TEST_ROOT/MJ_CACHE_TEST_NODE_MODULES")
	}
	base := os.Getenv("MJ_CACHE_TEST_ROOT")
	toolCacheMust(t, os.MkdirAll(base, 0o777))
	root, err := os.MkdirTemp(base, "build-caches-")
	toolCacheMust(t, err)
	modules := os.Getenv("MJ_CACHE_TEST_NODE_MODULES")
	if modules == "" {
		t.Fatal("environment variable not found")
	}
	a := filepath.Join(root, "checkout A")
	b := filepath.Join(root, "checkout B")
	for _, checkout := range []string{a, b} {
		toolCacheMust(t, toolCacheFixture(checkout, modules))
	}
	toolCacheMust(t, os.MkdirAll(filepath.Join(root, "home/.nx"), 0o777))
	ea := toolCacheTestEnvironment(root, a)
	eb := toolCacheTestEnvironment(root, b)
	run := func(cwd string, env map[string]string, command string, args ...string) string {
		t.Helper()
		output, err := toolCacheTestRun(cwd, env, command, args...)
		toolCacheMust(t, err)
		return output
	}
	for _, pair := range []struct {
		checkout string
		env      map[string]string
	}{{a, ea}, {b, eb}} {
		run(pair.checkout, pair.env, "git", "init", "-q")
		run(pair.checkout, pair.env, "git", "remote", "add", "origin", "https://cache-fixture.example/project.git")
		run(pair.checkout, pair.env, "git", "add", ".")
		run(pair.checkout, pair.env, "git", "-c", "user.name=Cache Test", "-c", "user.email=cache@example.test", "commit", "-qm", "fixture")
		toolCacheMust(t, toolCachePrepare(pair.checkout, pair.env))
	}
	for _, tool := range []struct {
		name string
		args []string
	}{
		{"go", []string{"version"}},
		{"gradle", []string{"--version"}},
		{"nx", []string{"--version"}},
		{"turbo", []string{"--version"}},
		{"bazelisk", []string{"--batch", "version"}},
		{"mbx", []string{"--version"}},
	} {
		fmt.Printf("%s: %s\n", tool.name, strings.TrimSpace(run(a, ea, tool.name, tool.args...)))
	}

	goBuild := []string{"build", "-x", "-o", "app", "."}
	run(a, ea, "go", goBuild...)
	cached := run(b, eb, "go", goBuild...)
	if strings.Contains(cached, "/compile -o") {
		t.Fatalf("Go recompiled across checkouts:\n%s", cached)
	}
	if strings.TrimSpace(run(b, eb, filepath.Join(b, "app"))) != "one" {
		t.Fatal("condition failed")
	}
	toolCacheMust(t, os.WriteFile(filepath.Join(b, "value.go"), []byte("package main\nconst value = \"two\"\n"), 0o666))
	run(b, eb, "go", goBuild...)
	if strings.TrimSpace(run(b, eb, filepath.Join(b, "app"))) != "two" {
		t.Fatal("condition failed")
	}
	fmt.Println("Go: second checkout reused compiled packages; input change rebuilt correctly")

	gradle := []string{"--no-daemon", "--max-workers=2", "--console=plain", "compileJava"}
	run(a, ea, "gradle", gradle...)
	cached = run(b, eb, "gradle", gradle...)
	if !strings.Contains(cached, ":compileJava FROM-CACHE") {
		t.Fatalf("Gradle missed:\n%s", cached)
	}
	original, err := os.ReadFile(filepath.Join(b, "build/classes/java/main/Value.class"))
	toolCacheMust(t, err)
	toolCacheMust(t, os.WriteFile(filepath.Join(b, "src/main/java/Value.java"), []byte("public class Value { public static final String VALUE = \"two\"; }\n"), 0o666))
	run(b, eb, "gradle", gradle...)
	rebuilt, err := os.ReadFile(filepath.Join(b, "build/classes/java/main/Value.class"))
	toolCacheMust(t, err)
	if string(rebuilt) == string(original) {
		t.Fatal("condition failed")
	}
	fmt.Println("Gradle: FROM-CACHE across private user homes; input change rebuilt correctly")

	for _, tool := range []struct {
		name   string
		args   []string
		output string
	}{
		{"turbo", []string{"run", "build"}, "turbo"},
		{"nx", []string{"run", "fixture:build", "--outputStyle=static"}, "nx"},
	} {
		run(a, ea, tool.name, tool.args...)
		cached := run(b, eb, tool.name, tool.args...)
		if _, err := os.Stat(filepath.Join(b, tool.output+"-calls.txt")); err == nil {
			t.Fatalf("%s executed in second checkout:\n%s", tool.name, cached)
		}
		value, err := os.ReadFile(filepath.Join(b, "out", tool.output, "value.txt"))
		toolCacheMust(t, err)
		if string(value) != "one\n" {
			t.Fatal("condition failed")
		}
		toolCacheMust(t, os.WriteFile(filepath.Join(b, "input.txt"), []byte("two\n"), 0o666))
		run(b, eb, tool.name, tool.args...)
		value, err = os.ReadFile(filepath.Join(b, "out", tool.output, "value.txt"))
		toolCacheMust(t, err)
		if string(value) != "two\n" {
			t.Fatal("condition failed")
		}
		toolCacheMust(t, os.WriteFile(filepath.Join(b, "input.txt"), []byte("one\n"), 0o666))
		fmt.Printf("%s: restored outputs without executing; input change rebuilt correctly\n", tool.name)
	}

	bazel := []string{"--batch", "build", "--enable_bzlmod=false", "--enable_workspace=true", "//:copy"}
	run(a, ea, "bazelisk", bazel...)
	cached = run(b, eb, "bazelisk", bazel...)
	if !strings.Contains(cached, "disk cache hit") {
		t.Fatalf("Bazel missed:\n%s", cached)
	}
	toolCacheMust(t, os.WriteFile(filepath.Join(b, "input.txt"), []byte("two\n"), 0o666))
	run(b, eb, "bazelisk", bazel...)
	copied, err := os.ReadFile(filepath.Join(b, "bazel-bin/copied.txt"))
	toolCacheMust(t, err)
	if string(copied) != "two\n" {
		t.Fatal("condition failed")
	}
	fmt.Println("Bazel: disk cache hit across output bases; input change rebuilt correctly")

	for _, pair := range []struct {
		checkout string
		env      map[string]string
	}{{a, ea}, {b, eb}} {
		run(pair.checkout, pair.env, "cmake", "-S", ".", "-B", "cmake-build", "-DCMAKE_C_COMPILER=/usr/bin/cc")
		result := run(pair.checkout, pair.env, "cmake", "--build", "cmake-build", "--", "-j2")
		fmt.Printf("CMake %s: %s\n", pair.checkout, result)
		if pair.checkout == b && !strings.Contains(result, "1 hits, 0 misses") {
			t.Fatalf("C/C++ missed:\n%s", result)
		}
		if strings.TrimSpace(run(pair.checkout, pair.env, filepath.Join(pair.checkout, "cmake-build/native"))) != "one" {
			t.Fatal("condition failed")
		}
	}
	// Shared in-flight writes must preserve each checkout's own outputs.
	for _, checkout := range []string{a, b} {
		toolCacheMust(t, os.WriteFile(filepath.Join(checkout, "value.go"), []byte("package main\nconst value = \"concurrent\"\n"), 0o666))
		toolCacheMust(t, os.WriteFile(filepath.Join(checkout, "input.txt"), []byte("concurrent\n"), 0o666))
		toolCacheMust(t, os.WriteFile(filepath.Join(checkout, "src/main/java/Value.java"), []byte("public class Value { public static final String VALUE = \"concurrent\"; }\n"), 0o666))
		toolCacheMust(t, os.WriteFile(filepath.Join(checkout, "native/native.c"), []byte("#include <stdio.h>\nint main(void) { puts(\"concurrent\"); }\n"), 0o666))
	}
	for _, tool := range []struct {
		name string
		args []string
	}{
		{"go", goBuild},
		{"gradle", gradle},
		{"turbo", []string{"run", "build"}},
		{"nx", []string{"run", "fixture:build", "--outputStyle=static"}},
		{"bazelisk", bazel},
		{"cmake", []string{"--build", "cmake-build", "--", "-j2"}},
	} {
		var group sync.WaitGroup
		failures := make([]error, 2)
		for index, pair := range []struct {
			checkout string
			env      map[string]string
		}{{a, ea}, {b, eb}} {
			group.Go(func() {
				_, failures[index] = toolCacheTestRun(pair.checkout, pair.env, tool.name, tool.args...)
			})
		}
		group.Wait()
		toolCacheMust(t, errors.Join(failures...))
		fmt.Printf("%s: concurrent independent builds succeeded\n", tool.name)
	}
	for _, pair := range []struct {
		checkout string
		env      map[string]string
	}{{a, ea}, {b, eb}} {
		for _, file := range []string{"out/turbo/value.txt", "out/nx/value.txt", "bazel-bin/copied.txt"} {
			value, err := os.ReadFile(filepath.Join(pair.checkout, file))
			toolCacheMust(t, err)
			if string(value) != "concurrent\n" {
				t.Fatalf("%s %s", pair.checkout, file)
			}
		}
		for _, file := range []string{"app", "cmake-build/native"} {
			if strings.TrimSpace(run(pair.checkout, pair.env, filepath.Join(pair.checkout, file))) != "concurrent" {
				t.Fatal("condition failed")
			}
		}
	}
	fmt.Printf("Evidence retained in %s\n", root)
}

func toolCacheFixture(root, modules string) error {
	if err := os.MkdirAll(filepath.Join(root, "src/main/java"), 0o777); err != nil {
		return err
	}
	if err := os.MkdirAll(filepath.Join(root, "native"), 0o777); err != nil {
		return err
	}
	for _, file := range [][2]string{
		{".gitignore", "node_modules\nout\n*-calls.txt\n.nx\n.turbo\n.gradle\nbuild\nbazel-*\ncmake-build\napp\n"},
		{"go.mod", "module cachetest.example/check\n\ngo 1.23\n"},
		{"main.go", "package main\nimport \"fmt\"\nfunc main() { fmt.Println(value) }\n"},
		{"value.go", "package main\nconst value = \"one\"\n"},
		{"settings.gradle", "rootProject.name = 'cache-fixture'\n"},
		{"build.gradle", "plugins { id 'java' }\n"},
		{"src/main/java/Value.java", "public class Value { public static final String VALUE = \"one\"; }\n"},
		{"input.txt", "one\n"},
		{"package.json", `{"name":"fixture","version":"1.0.0","private":true,"packageManager":"npm@10.8.2","scripts":{"build":"node build.cjs web"},"devDependencies":{"nx":"23.3.0","turbo":"2.11.7"}}`},
		{"package-lock.json", `{"name":"fixture","version":"1.0.0","lockfileVersion":3,"packages":{"":{"name":"fixture","version":"1.0.0"}}}`},
		{"turbo.json", `{"tasks":{"build":{"inputs":["input.txt","build.cjs"],"outputs":["out/turbo/**"]}}}`},
		{"nx.json", `{"defaultBase":"master","targetDefaults":{"build":{"cache":true}}}`},
		{"project.json", `{"name":"fixture","targets":{"build":{"executor":"nx:run-commands","cache":true,"inputs":["{workspaceRoot}/input.txt","{workspaceRoot}/build.cjs"],"outputs":["{workspaceRoot}/out/nx"],"options":{"command":"node build.cjs nx"}}}}`},
		{"build.cjs", "const fs=require('fs'); const name=process.argv[2]==='web'?'turbo':process.argv[2]; fs.mkdirSync('out/'+name,{recursive:true}); fs.copyFileSync('input.txt','out/'+name+'/value.txt'); fs.appendFileSync(name+'-calls.txt','executed\\n');\n"},
		{".bazelversion", "8.3.1\n"},
		{"WORKSPACE", "workspace(name = \"cache_fixture\")\n"},
		{"BUILD", "genrule(name = \"copy\", srcs = [\"input.txt\"], outs = [\"copied.txt\"], cmd = \"cp $(location input.txt) $@\")\n"},
		{"CMakeLists.txt", "cmake_minimum_required(VERSION 3.16)\nproject(CacheFixture C)\nadd_executable(native native/native.c)\n"},
		{"native/native.c", "#include <stdio.h>\nint main(void) { puts(\"one\"); }\n"},
	} {
		if err := os.WriteFile(filepath.Join(root, file[0]), []byte(file[1]), 0o666); err != nil {
			return err
		}
	}
	return os.Symlink(modules, filepath.Join(root, "node_modules"))
}
