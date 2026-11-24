//go:build linux

// Port of the tests in mj-build-cache/src/mbx/configuration.rs
// (`#[cfg(all(test, target_os = "linux"))]`), with its test-only
// DEFAULT_MARKER and managed_document.

package buildcache

import (
	"fmt"
	"os"
	"strconv"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const defaultMarker = "# mj automatic shared budget: "

// managedDocument is the managed machine policy document Rust's tests
// serialize with the toml crate: `[gc]` with the total, then `[scheduler]`
// when it sets anything.
func managedDocument(settings core.TargetBuildCache, automatic string) string {
	total := automatic
	if settings.MaxTotalSize != nil {
		total = *settings.MaxTotalSize
	}
	document := "[gc]\nmax_total_size = " + strconv.Quote(total) + "\n"
	if !settings.Scheduler.IsDefault() {
		document += "\n[scheduler]\n"
		if settings.Scheduler.CPUs != nil {
			document += fmt.Sprintf("cpus = %d\n", *settings.Scheduler.CPUs)
		}
		if settings.Scheduler.Memory != nil {
			document += "memory = " + strconv.Quote(*settings.Scheduler.Memory) + "\n"
		}
	}
	return "# Managed by Mjolnir. Change the machine's Build cache settings.\n" + defaultMarker + automatic + "\n" + document
}

func configurationCache(directory string, previous *string, text string) *ResolvedBuildCache {
	return &ResolvedBuildCache{
		Directory:       directory,
		NativeMbx:       NativeMbx{Program: "/usr/local/bin/mbx", Version: MbxVersion},
		ConfigDirectory: configurationSharedDirectory(directory),
		ConfigFile:      &text,
		PreviousConfig:  previous,
	}
}

func TestBothConsumersSeeAtomicBudgetUpdatesWithoutRelinking(t *testing.T) {
	requireLinux(t)
	temporary := t.TempDir()
	executor := core.ProcessExecutor{}
	first := managedDocument(core.TargetBuildCache{MaxTotalSize: stringPointer("500GiB")}, "100GB")
	initial := configurationCache(temporary, nil, first)
	if applied, err := configurationApply(CacheHost{}, initial, executor); err != nil || !applied {
		t.Fatalf("applied = %v, %v", applied, err)
	}
	file := pathJoin(initial.ConfigDirectory, "config.toml")
	paths := []string{pathJoin(temporary, "harness.toml"), pathJoin(temporary, "reviewer.toml")}
	for _, path := range paths {
		if err := os.Symlink(file, path); err != nil {
			t.Fatal(err)
		}
	}
	second := managedDocument(core.TargetBuildCache{MaxTotalSize: stringPointer("100GB")}, "100GB")
	// Exercise the shared subprocess stdin path beyond a pipe buffer.
	second = second + "# " + strings.Repeat("x", 256*1024) + "\n"
	updated := configurationCache(temporary, &first, second)
	if applied, err := configurationApply(CacheHost{}, updated, executor); err != nil || !applied {
		t.Fatalf("applied = %v, %v", applied, err)
	}
	for _, path := range paths {
		text := readText(t, path)
		if text != second {
			t.Fatalf("%s does not hold the new policy", path)
		}
		limit, err := configurationConfiguredLimit(&text, "target", "max_size")
		if err != nil || limit != nil {
			t.Fatalf("target.max_size = %s, %v", optionalText(limit), err)
		}
		limit, err = configurationConfiguredLimit(&text, "gc", "max_total_size")
		if err != nil || limit == nil || *limit != "100GB" {
			t.Fatalf("gc.max_total_size = %s, %v", optionalText(limit), err)
		}
	}
	before := inode(t, file)
	if applied, err := configurationApply(CacheHost{}, configurationCache(temporary, &second, second), executor); err != nil || !applied {
		t.Fatalf("applied = %v, %v", applied, err)
	}
	if inode(t, file) != before {
		t.Fatal("unchanged policy must not be rewritten")
	}
}

func TestExistingCacheMountsReceiveMachineChangesWithoutChangingTheSource(t *testing.T) {
	requireLinux(t)
	temporary := t.TempDir()
	host := CacheHost{}
	executor := core.ProcessExecutor{}
	source := pathJoin(temporary, "native.toml")
	current := pathJoin(temporary, "current-cache")
	existing := pathJoin(temporary, "existing-container-cache")
	for _, budget := range []string{"150GiB", "250GiB"} {
		text := "[target]\nmax_size = '" + budget + "'\n"
		if err := os.WriteFile(source, []byte(text), 0o666); err != nil {
			t.Fatal(err)
		}
		policy := configurationCache(current, nil, text)
		if _, err := publishAt(host, policy, existing, executor); err != nil {
			t.Fatal(err)
		}
		if readText(t, source) != text {
			t.Fatal("the native source changed")
		}
		if readText(t, pathJoin(configurationSharedDirectory(existing), "config.toml")) != text {
			t.Fatal("the existing mount did not receive the machine policy")
		}
	}
}

func TestStaleApplicationCannotOverwriteANewerMachinePolicy(t *testing.T) {
	requireLinux(t)
	temporary := t.TempDir()
	executor := core.ProcessExecutor{}
	first := configurationCache(temporary, nil, "[gc]\nmax_total_size = '500GiB'\n")
	stale := configurationCache(temporary, nil, "[gc]\nmax_total_size = '1GiB'\n")
	if applied, err := configurationApply(CacheHost{}, first, executor); err != nil || !applied {
		t.Fatalf("applied = %v, %v", applied, err)
	}
	if applied, err := configurationApply(CacheHost{}, stale, executor); err != nil || applied {
		t.Fatalf("a stale application applied = %v, %v", applied, err)
	}
	if readText(t, pathJoin(first.ConfigDirectory, "config.toml")) != *first.ConfigFile {
		t.Fatal("the newer policy was overwritten")
	}
}

// Hard-won: #1580: concurrent launches applying the same policy from one inspection failed the loser
func TestLaunchesApplyingTheSamePolicyFromOneInspectionBothSucceed(t *testing.T) {
	requireLinux(t)
	temporary := t.TempDir()
	executor := core.ProcessExecutor{}
	policy := "[gc]\nmax_total_size = '500GiB'\n"
	file := pathJoin(configurationSharedDirectory(temporary), "config.toml")
	// Both launches inspected the host before either applied.
	inspection := configurationCache(temporary, nil, policy)
	if applied, err := configurationApply(CacheHost{}, inspection, executor); err != nil || !applied {
		t.Fatalf("applied = %v, %v", applied, err)
	}
	before := inode(t, file)
	if applied, err := configurationApply(CacheHost{}, inspection, executor); err != nil || !applied {
		t.Fatalf("the second launch applied = %v, %v", applied, err)
	}
	if readText(t, file) != policy {
		t.Fatal("the policy changed")
	}
	if inode(t, file) != before {
		t.Fatal("the same policy was rewritten")
	}
}
