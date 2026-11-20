// Tests of the Linux part of available_parallelism: the path handling of
// Rust's cgroups module.

package rsthread

import "testing"

func TestThread_port_cgroup_paths_behave_as_pathbuf(t *testing.T) {
	assertEq(t, pathPush("/sys/fs/cgroup", ""), "/sys/fs/cgroup/")
	assertEq(t, pathPush("/sys/fs/cgroup/", "cpu.max"), "/sys/fs/cgroup/cpu.max")
	assertEq(t, pathPush("/sys/fs/cgroup", "user.slice/x.scope"), "/sys/fs/cgroup/user.slice/x.scope")
	assertEq(t, pathPush("/a", "/b"), "/b")
	assertEq(t, pathPop("/sys/fs/cgroup/"), "/sys/fs")
	assertEq(t, pathPop("/sys/fs/cgroup/cpu.max"), "/sys/fs/cgroup")
	assertEq(t, pathPop("/"), "/")
	assert(t, pathStartsWith("/sys/fs/cgroup", "/sys/fs/cgroup"))
	assert(t, pathStartsWith("/sys/fs/cgroup//x/./y", "/sys/fs/cgroup"))
	assert(t, !pathStartsWith("/sys/fs/cgroupx", "/sys/fs/cgroup"))
	assert(t, !pathStartsWith("/sys/fs", "/sys/fs/cgroup"))
	assert(t, pathStartsWith("/sys/fs/cgroup/../../x", "/sys/fs/cgroup"))
	assert(t, pathStartsWith("a/b", ""))
	assertEq(t, pathStripPrefix("a/b/c", "a"), "b/c")
	for text, want := range map[string]int{"100000": 100000, "+5": 5, "0": 0} {
		got, ok := parseUsize(text)
		assert(t, ok && got == want, text)
	}
	for _, text := range []string{"", "+", "-1", "max", " 1", "1 ", "99999999999999999999"} {
		_, ok := parseUsize(text)
		assert(t, !ok, text)
	}
}
