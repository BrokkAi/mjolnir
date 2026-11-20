// Port of the tests in mj-core/src/targets.rs that run on every platform.

package core

import (
	"slices"
	"testing"
)

func TestPodmanExecKeepsTheContainerConfiguredUser(t *testing.T) {
	cases := [][]string{
		{"exec", "--detach", "session", "true"},
		{"exec", "--user", "1000:1000", "session", "id"},
		{"exec", "session", "tool", "--user", "42"},
	}
	for _, args := range cases {
		if got := NewCommandSpec("podman", args).Args; !slices.Equal(got, args) {
			t.Fatalf("podman %v became %v", args, got)
		}
	}
	args := []string{"exec", "session", "id"}
	if got := NewCommandSpec("docker", args).Args; !slices.Equal(got, args) {
		t.Fatalf("docker %v became %v", args, got)
	}
}
