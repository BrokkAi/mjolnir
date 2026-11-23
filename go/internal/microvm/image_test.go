//go:build linux

// Port of the tests of mj-microvm/src/image.rs.

package microvm

import (
	"encoding/json"
	"fmt"
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"testing"
)

func microvmWriteTestImage(t *testing.T, layout MicrovmLayout, id string, architecture string) {
	t.Helper()
	microvmMustMkdirAll(t, layout.Image(id))
	microvmMustWrite(t, filepath.Join(layout.Image(id), "root.ext4"), []byte("disk"))
	config, err := json.Marshal(MicrovmImageConfig{
		Format:       MicrovmMetadataFormat,
		ID:           id,
		Architecture: architecture,
		Env:          []string{},
		Entrypoint:   []string{},
		Cmd:          []string{},
		Labels:       map[string]string{},
		RootBytes:    4,
	})
	if err != nil {
		t.Fatal(err)
	}
	microvmMustWrite(t, filepath.Join(layout.Image(id), "config.json"), config)
}

func microvmTestHostArchitecture(t *testing.T) string {
	t.Helper()
	host, err := MicrovmHostArchitecture()
	if err != nil {
		t.Fatal(err)
	}
	return host
}

// An engine that predates the architecture check may have cached a
// single-platform image for another architecture; it must not boot.
func TestACachedImageForAnotherArchitectureIsNotReused(t *testing.T) {
	layout := NewMicrovmLayout(t.TempDir())
	if err := layout.Ensure(); err != nil {
		t.Fatal(err)
	}
	other := "amd64"
	if microvmTestHostArchitecture(t) == "amd64" {
		other = "arm64"
	}
	id := strings.Repeat("d", 64)
	microvmWriteTestImage(t, layout, id, other)
	refs := MicrovmImageRefs{Format: MicrovmMetadataFormat, Refs: map[string]string{"foreign:latest": id}}
	if err := refs.Write(layout); err != nil {
		t.Fatal(err)
	}

	_, _, err := MicrovmEnsureImage(layout, "foreign:latest", nil, MicrovmPullPolicyNever)
	if err == nil || !strings.Contains(err.Error(), fmt.Sprintf("is linux/%s, but this Nitro host runs", other)) {
		t.Fatalf("%v", err)
	}
}

func TestMaintainKeepsTheInUseDiskAndRemovesAnUnkeptUnusedReference(t *testing.T) {
	layout := NewMicrovmLayout(t.TempDir())
	if err := layout.Ensure(); err != nil {
		t.Fatal(err)
	}
	live := strings.Repeat("a", 64)
	unused := strings.Repeat("b", 64)
	for _, id := range []string{live, unused} {
		microvmWriteTestImage(t, layout, id, microvmTestHostArchitecture(t))
	}
	refs := MicrovmImageRefs{Format: MicrovmMetadataFormat, Refs: map[string]string{
		"live:latest":   live,
		"unused:latest": unused,
	}}
	if err := refs.Write(layout); err != nil {
		t.Fatal(err)
	}
	microvmMustMkdirAll(t, layout.VM("live"))
	state := MicrovmVmState{
		Format:         MicrovmStateFormat,
		ID:             strings.Repeat("c", 64),
		Name:           "live",
		ImageReference: "live:latest",
		ImageID:        live,
		Labels:         map[string]string{},
		VCPUs:          1,
		NanoCPUs:       1_000_000_000,
		MemoryBytes:    1,
		Env:            []string{},
		Entrypoint:     []string{},
		Cmd:            []string{},
		Volumes:        []MicrovmVmVolume{},
		Slot:           0,
		EngineBuild:    "test",
	}
	if err := state.Write(layout); err != nil {
		t.Fatal(err)
	}

	removed, err := MicrovmImageReconcileReferences(layout, nil)
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(removed, []string{"live:latest", "unused:latest"}) {
		t.Fatalf("removed %q", removed)
	}
	pruned, err := MicrovmImagePrune(layout)
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Contains(pruned, unused) || slices.Contains(pruned, live) {
		t.Fatalf("pruned %q", pruned)
	}
	if !microvmExists(layout.Image(live)) || microvmExists(layout.Image(unused)) {
		t.Fatal("prune removed the in-use image or kept the unused one")
	}
	after, err := MicrovmImageRefsRead(layout)
	if err != nil {
		t.Fatal(err)
	}
	if len(after.Refs) != 0 {
		t.Fatalf("refs left: %v", after.Refs)
	}
}
