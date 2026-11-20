//go:build linux

// Port of mj-worker/src/microvm/image.rs.
//
// OCI images as read-only ext4 base images.
//
// `crane` exports an image's filesystem and configuration without a
// container engine. The engine extracts the filesystem as root (so
// ownership survives), builds an ext4 image from it with `mkfs.ext4 -d`, and
// keeps both next to the image's configuration. Each VM gets a reflink clone
// of the base as its writable root disk. The image id is the SHA-256 of the
// OCI configuration blob, the id Podman reports.
//
// The OCI configuration is a JSON value as
// github.com/BrokkAi/acp-go/jsonvalue holds it.

package worker

import (
	"encoding/hex"
	"fmt"
	"os/exec"
	"path/filepath"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// MicrovmPullPolicy is what `--pull` asks for.
type MicrovmPullPolicy int

const (
	MicrovmPullPolicyAlways MicrovmPullPolicy = iota
	MicrovmPullPolicyMissing
	MicrovmPullPolicyNever
)

// MicrovmParsePullPolicy is PullPolicy::parse.
func MicrovmParsePullPolicy(value string) (MicrovmPullPolicy, error) {
	switch value {
	case "always", "newer":
		return MicrovmPullPolicyAlways, nil
	case "missing":
		return MicrovmPullPolicyMissing, nil
	case "never":
		return MicrovmPullPolicyNever, nil
	}
	return 0, fmt.Errorf("unsupported --pull value %s; use always, missing, never or newer", microvmDebugString(value))
}

// MicrovmNormalizeReference adds the default tag so `alpine` and
// `alpine:latest` name one image.
func MicrovmNormalizeReference(reference string) string {
	nameStart := strings.LastIndex(reference, "/") + 1
	if strings.Contains(reference, "@") || strings.Contains(reference[nameStart:], ":") {
		return reference
	}
	return reference + ":latest"
}

// MicrovmEnsureImage resolves reference to an image id, pulling according
// to policy. The returned lock reserves the image against a prune until the
// caller has recorded its use of it.
func MicrovmEnsureImage(layout MicrovmLayout, reference string, platform *string, policy MicrovmPullPolicy) (string, *core.FileLock, error) {
	panic("unported: mj-worker/src/microvm/image.rs ensure_image")
}

// MicrovmImageLookup is the id a reference or id names on this host; ok is
// false when it names none.
func MicrovmImageLookup(layout MicrovmLayout, reference string) (id string, ok bool, err error) {
	panic("unported: mj-worker/src/microvm/image.rs lookup")
}

func microvmImagePlatformFor(requested *string) (string, error) {
	panic("unported: mj-worker/src/microvm/image.rs platform_for")
}

// MicrovmImagePull fetches reference and flattens it unless this host
// already holds that exact image.
func MicrovmImagePull(layout MicrovmLayout, reference string, platform *string) (string, error) {
	id, lock, err := microvmImagePullReserved(layout, reference, platform)
	if err != nil {
		return "", err
	}
	lock.Drop()
	return id, nil
}

// microvmImagePullReserved is MicrovmImagePull, returning the image's lock
// so the caller can keep the image from being pruned until it has recorded
// its own use of it.
func microvmImagePullReserved(layout MicrovmLayout, reference string, platform *string) (string, *core.FileLock, error) {
	panic("unported: mj-worker/src/microvm/image.rs pull_reserved")
}

func microvmImageFirstNumber(output []byte) (uint64, bool) {
	panic("unported: mj-worker/src/microvm/image.rs first_number")
}

func microvmImageHex(data []byte) string {
	return hex.EncodeToString(data)
}

// microvmImageStrings is the strings in the array value[key], skipping
// members that are not strings.
func microvmImageStrings(value any, key string) []string {
	panic("unported: mj-worker/src/microvm/image.rs strings")
}

func microvmImageFlatten(layout MicrovmLayout, crane string, reference string, platform string, id string, config any) error {
	panic("unported: mj-worker/src/microvm/image.rs flatten")
}

func microvmImageFlattenInto(scratch string, crane string, reference string, platform string, id string, config any) error {
	panic("unported: mj-worker/src/microvm/image.rs flatten_into")
}

// microvmImageExtract is `crane export | tar -x`. The two processes share a
// pipe the parent never touches, and each one's diagnostics go to a file,
// so nothing here can block on an unread pipe.
func microvmImageExtract(crane string, reference string, platform string, scratch string, rootfs string) error {
	panic("unported: mj-worker/src/microvm/image.rs extract")
}

// microvmImageStopExporter ends an exporter whose consumer never started,
// and reaps it.
func microvmImageStopExporter(exporter *exec.Cmd) {
	panic("unported: mj-worker/src/microvm/image.rs stop_exporter")
}

// MicrovmImagePrune removes images nothing refers to, and scratch left by
// interrupted pulls. An image is in use if a reference names it, a VM was
// created from it, or a staged root replacement is about to use it.
func MicrovmImagePrune(layout MicrovmLayout) ([]string, error) {
	panic("unported: mj-worker/src/microvm/image.rs prune")
}

// MicrovmRootImage is the path to an image's root disk.
func MicrovmRootImage(layout MicrovmLayout, id string) string {
	return filepath.Join(layout.Image(id), "root.ext4")
}
