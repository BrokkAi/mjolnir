//go:build linux

// Port of mj-microvm/src/image.rs.
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

package microvm

import (
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"fmt"
	"log/slog"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/jsonvalue"

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
	// A requested platform this host cannot run is refused even when an
	// image is already present: that copy is the host's architecture.
	if _, err := microvmImagePlatformFor(platform); err != nil {
		return "", nil, err
	}
	if err := layout.Ensure(); err != nil {
		return "", nil, err
	}
	if policy != MicrovmPullPolicyAlways {
		id, ok, err := MicrovmImageLookup(layout, reference)
		if err != nil {
			return "", nil, err
		}
		if ok {
			// Name the image and reserve it in one step: a prune decides under
			// this lock, so it cannot remove the image between the two.
			lock, err := layout.LockImage(id)
			if err != nil {
				return "", nil, err
			}
			if microvmIsFile(filepath.Join(layout.Image(id), "config.json")) {
				return id, lock, nil
			}
			// Pruned while we waited for the lock.
			err = layout.ForgetImageLock(id, lock)
			lock.Drop()
			if err != nil {
				return "", nil, err
			}
		}
	}
	if policy == MicrovmPullPolicyNever {
		return "", nil, fmt.Errorf("%s: image not known", reference)
	}
	return microvmImagePullReserved(layout, reference, platform)
}

// MicrovmImageLookup is the id a reference or id names on this host; ok is
// false when it names none.
func MicrovmImageLookup(layout MicrovmLayout, reference string) (id string, ok bool, err error) {
	refs, err := MicrovmImageRefsRead(layout)
	if err != nil {
		return "", false, err
	}
	if id, ok := refs.Refs[MicrovmNormalizeReference(reference)]; ok {
		return id, true, nil
	}
	// A full image id, or an unambiguous prefix of one, also names an image.
	hexadecimal := len(reference) >= 4
	for i := 0; hexadecimal && i < len(reference); i++ {
		hexadecimal = microvmIsASCIIHexDigit(reference[i])
	}
	if hexadecimal {
		var matches []string
		names, _ := microvmReadDirNames(layout.Images())
		wanted := strings.ToLower(reference)
		for _, name := range names {
			if strings.HasPrefix(name, wanted) && microvmIsFile(filepath.Join(layout.Image(name), "config.json")) {
				matches = append(matches, name)
			}
		}
		if len(matches) == 1 {
			return matches[0], true, nil
		}
	}
	return "", false, nil
}

func microvmImagePlatformFor(requested *string) (string, error) {
	host, err := MicrovmHostArchitecture()
	if err != nil {
		return "", err
	}
	platform := "linux/" + host
	if requested != nil {
		platform = *requested
	}
	system, architecture, ok := strings.Cut(platform, "/")
	if !ok {
		return "", fmt.Errorf("invalid platform %s; use os/architecture", microvmDebugString(platform))
	}
	if system != "linux" || architecture != host {
		return "", fmt.Errorf("platform %s cannot run on this host: a microVM runs linux/%s only", platform, host)
	}
	return platform, nil
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
	if err := layout.Ensure(); err != nil {
		return "", nil, err
	}
	resolvedPlatform, err := microvmImagePlatformFor(platform)
	if err != nil {
		return "", nil, err
	}
	crane, err := MicrovmFindTool("crane")
	if err != nil {
		return "", nil, err
	}
	// Resolve a mutable tag once. The configuration and the filesystem are
	// both fetched by digest, so a tag that moves in between cannot pair one
	// image's configuration with another's files.
	digest, err := MicrovmRunChecked(crane, []string{"digest", "--full-ref", reference})
	if err != nil {
		return "", nil, fmt.Errorf("resolve %s: %w", reference, err)
	}
	if !utf8.Valid(digest.Stdout) {
		return "", nil, errors.New("crane printed a digest that is not UTF-8")
	}
	pinned := strings.TrimSpace(string(digest.Stdout))
	configuration, err := MicrovmRunChecked(crane, []string{"config", "--platform", resolvedPlatform, pinned})
	if err != nil {
		return "", nil, fmt.Errorf("fetch the configuration of %s: %w", pinned, err)
	}
	sum := sha256.Sum256(configuration.Stdout)
	id := microvmImageHex(sum[:])
	config, err := jsonvalue.Parse(configuration.Stdout)
	if err != nil {
		return "", nil, fmt.Errorf("decode the OCI image configuration: %w", err)
	}
	// The image lock covers flattening and recording the reference, so a
	// prune (which decides under the same lock) never sees the image between
	// the two. Lock order everywhere is image, then global.
	imageLock, err := layout.LockImage(id)
	if err != nil {
		return "", nil, err
	}
	if !microvmIsFile(filepath.Join(layout.Image(id), "config.json")) {
		if err := microvmImageFlatten(layout, crane, pinned, resolvedPlatform, id, config); err != nil {
			// Nothing was published, so no image keeps this lock file.
			if forget := layout.ForgetImageLock(id, imageLock); forget != nil {
				slog.Warn("could not remove an image lock file", "error", forget)
			}
			imageLock.Drop()
			return "", nil, fmt.Errorf("flatten %s: %w", reference, err)
		}
	}
	recorded := func() error {
		global, err := layout.LockGlobal()
		if err != nil {
			return err
		}
		defer global.Drop()
		refs, err := MicrovmImageRefsRead(layout)
		if err != nil {
			return err
		}
		refs.Format = MicrovmMetadataFormat
		refs.Refs[MicrovmNormalizeReference(reference)] = id
		return refs.Write(layout)
	}()
	if recorded != nil {
		imageLock.Drop()
		return "", nil, recorded
	}
	return id, imageLock, nil
}

func microvmImageFirstNumber(output []byte) (uint64, bool) {
	end := 0
	for end < len(output) && !microvmIsASCIIWhitespace(output[end]) {
		end++
	}
	field := output[:end]
	if !utf8.Valid(field) {
		return 0, false
	}
	value, err := microvmParseUint(string(field), 64)
	return value, err == nil
}

// microvmIsASCIIWhitespace is Rust's u8::is_ascii_whitespace.
func microvmIsASCIIWhitespace(b byte) bool {
	return b == ' ' || b == '\t' || b == '\n' || b == '\x0c' || b == '\r'
}

func microvmImageHex(data []byte) string {
	return hex.EncodeToString(data)
}

// microvmImageStrings is the strings in the array value[key], skipping
// members that are not strings.
func microvmImageStrings(value any, key string) []string {
	object, ok := value.(jsonvalue.Object)
	if !ok {
		return []string{}
	}
	items, ok := microvmObjectGet(object, key).([]any)
	if !ok {
		return []string{}
	}
	strings := []string{}
	for _, item := range items {
		if text, ok := item.(string); ok {
			strings = append(strings, text)
		}
	}
	return strings
}

// microvmImageString is value[key] when it is a string, or "".
func microvmImageString(value any, key string) string {
	object, ok := value.(jsonvalue.Object)
	if !ok {
		return ""
	}
	text, _ := microvmObjectGet(object, key).(string)
	return text
}

func microvmImageFlatten(layout MicrovmLayout, crane string, reference string, platform string, id string, config any) error {
	scratch := filepath.Join(layout.Images(), fmt.Sprintf(".tmp-%s-%s", id, MicrovmScratchSuffix()))
	if err := os.MkdirAll(filepath.Join(scratch, "rootfs"), 0o777); err != nil {
		return fmt.Errorf("create %s: %w", scratch, microvmIOError(err))
	}
	if err := microvmImageFlattenInto(scratch, crane, reference, platform, id, config); err != nil {
		// The scratch tree is ours alone; the processes that wrote it have exited.
		if cleanup := os.RemoveAll(scratch); cleanup != nil {
			slog.Warn("could not remove image scratch directory", "error", cleanup, "scratch", scratch)
		}
		return err
	}
	if err := os.Rename(scratch, layout.Image(id)); err != nil {
		return fmt.Errorf("publish image %s: %w", id, microvmIOError(err))
	}
	return nil
}

func microvmImageFlattenInto(scratch string, crane string, reference string, platform string, id string, config any) error {
	rootfs := filepath.Join(scratch, "rootfs")
	if err := microvmImageExtract(crane, reference, platform, scratch, rootfs); err != nil {
		return err
	}
	for _, directory := range []struct {
		name string
		mode os.FileMode
	}{
		{"proc", 0o755},
		{"sys", 0o755},
		{"dev", 0o755},
		{"run", 0o755},
		{"tmp", 0o1777},
	} {
		path := filepath.Join(rootfs, directory.name)
		if !microvmExists(path) {
			if err := os.Mkdir(path, 0o777); err != nil {
				return fmt.Errorf("create %s: %w", path, microvmIOError(err))
			}
			mode := directory.mode.Perm()
			if directory.mode&0o1000 != 0 {
				mode |= os.ModeSticky
			}
			if err := os.Chmod(path, mode); err != nil {
				return microvmIOError(err)
			}
		}
	}
	du, err := MicrovmFindTool("du")
	if err != nil {
		return err
	}
	usage, err := MicrovmRunChecked(du, []string{"-sb", "--apparent-size", rootfs})
	if err != nil {
		return err
	}
	bytes, ok := microvmImageFirstNumber(usage.Stdout)
	if !ok {
		return errors.New("read the size of the extracted image")
	}
	// The root disk is sparse, so it is sized like the volumes: to the
	// filesystem that holds it. Its real limit is the host's free space, and
	// a size chosen here would be baked into every VM cloned from the image.
	filesystem, err := MicrovmFilesystemBytes(scratch)
	if err != nil {
		return err
	}
	size := filesystem >> 20 << 20
	if size <= bytes+(1<<30) {
		return fmt.Errorf("the image filesystem (%d MiB) is too small for this image (%d MiB)", size>>20, bytes>>20)
	}
	rootImage := filepath.Join(scratch, "root.ext4")
	file, err := os.Create(rootImage)
	if err != nil {
		return microvmIOError(err)
	}
	err = file.Truncate(int64(size))
	_ = file.Close()
	if err != nil {
		return microvmIOError(err)
	}
	if _, err := MicrovmRunTool("mkfs.ext4", []string{
		"-q",
		"-F",
		"-m",
		"0",
		"-L",
		"rootfs",
		"-E",
		"root_owner=0:0,lazy_itable_init=1,lazy_journal_init=1,nodiscard",
		"-d",
		rootfs,
		rootImage,
	}); err != nil {
		return err
	}
	if err := os.RemoveAll(rootfs); err != nil {
		return fmt.Errorf("remove the extracted tree: %w", microvmIOError(err))
	}
	var inner any
	if object, ok := config.(jsonvalue.Object); ok {
		inner = microvmObjectGet(object, "config")
	}
	labels := map[string]string{}
	if object, ok := inner.(jsonvalue.Object); ok {
		if labelObject, ok := microvmObjectGet(object, "Labels").(jsonvalue.Object); ok {
			for _, member := range labelObject {
				if value, ok := member.Value.(string); ok {
					labels[member.Key] = value
				}
			}
		}
	}
	image := MicrovmImageConfig{
		Format:       MicrovmMetadataFormat,
		ID:           id,
		Architecture: microvmImageString(config, "architecture"),
		Env:          microvmImageStrings(inner, "Env"),
		User:         microvmImageString(inner, "User"),
		WorkingDir:   microvmImageString(inner, "WorkingDir"),
		Entrypoint:   microvmImageStrings(inner, "Entrypoint"),
		Cmd:          microvmImageStrings(inner, "Cmd"),
		Labels:       labels,
		RootBytes:    size,
	}
	return MicrovmWriteJSONAtomic(filepath.Join(scratch, "config.json"), image)
}

// microvmImageExtract is `crane export | tar -x`. The two processes share a
// pipe the parent never touches, and each one's diagnostics go to a file,
// so nothing here can block on an unread pipe.
func microvmImageExtract(crane string, reference string, platform string, scratch string, rootfs string) error {
	craneLog := filepath.Join(scratch, "crane.log")
	tarLog := filepath.Join(scratch, "tar.log")
	// Everything that can fail before the exporter starts happens first, so
	// no early return leaves it running.
	tarProgram, err := MicrovmFindTool("tar")
	if err != nil {
		return err
	}
	craneLogFile, err := os.Create(craneLog)
	if err != nil {
		return microvmIOError(err)
	}
	defer craneLogFile.Close()
	tarLogFile, err := os.Create(tarLog)
	if err != nil {
		return microvmIOError(err)
	}
	defer tarLogFile.Close()
	pipeRead, pipeWrite, err := os.Pipe()
	if err != nil {
		return fmt.Errorf("start crane export: %w", microvmIOError(err))
	}
	exporter := exec.Command(crane, "export", "--platform", platform, reference, "-")
	exporter.Stdout = pipeWrite
	exporter.Stderr = craneLogFile
	err = exporter.Start()
	_ = pipeWrite.Close()
	if err != nil {
		_ = pipeRead.Close()
		return fmt.Errorf("start crane export: %w", microvmIOError(err))
	}
	unpacker := exec.Command(tarProgram, "-x", "--numeric-owner", "--xattrs", "--xattrs-include=*", "-p", "-C", rootfs)
	unpacker.Stdin = pipeRead
	unpacker.Stderr = tarLogFile
	err = unpacker.Start()
	_ = pipeRead.Close()
	if err != nil {
		microvmImageStopExporter(exporter)
		return fmt.Errorf("run tar: %w", microvmIOError(err))
	}
	unpacked, unpackErr := microvmWaitStatus(unpacker)
	exported, exportErr := microvmWaitStatus(exporter)
	if unpackErr != nil {
		return fmt.Errorf("run tar: %w", microvmIOError(unpackErr))
	}
	if exportErr != nil {
		return fmt.Errorf("wait for crane export: %w", microvmIOError(exportErr))
	}
	if !exported.Success() {
		log, _ := os.ReadFile(craneLog)
		return fmt.Errorf("crane export failed (%s): %s", microvmExitStatus(exported), MicrovmTail(microvmUTF8OrEmpty(log), 10))
	}
	if !unpacked.Success() {
		log, _ := os.ReadFile(tarLog)
		return fmt.Errorf("extracting the image failed (%s): %s", microvmExitStatus(unpacked), MicrovmTail(microvmUTF8OrEmpty(log), 10))
	}
	return nil
}

// microvmUTF8OrEmpty is Rust's `fs::read_to_string(..).unwrap_or_default()`
// on bytes already read: text that is not UTF-8 reads as nothing.
func microvmUTF8OrEmpty(data []byte) string {
	if !utf8.Valid(data) {
		return ""
	}
	return string(data)
}

// microvmWaitStatus waits for a started command. A nonzero exit or a
// signal is a status, as Rust's ExitStatus is, not an error.
func microvmWaitStatus(command *exec.Cmd) (*os.ProcessState, error) {
	err := command.Wait()
	var exit *exec.ExitError
	if err != nil && !errors.As(err, &exit) {
		return nil, err
	}
	return command.ProcessState, nil
}

// microvmImageStopExporter ends an exporter whose consumer never started,
// and reaps it.
func microvmImageStopExporter(exporter *exec.Cmd) {
	if err := exporter.Process.Kill(); err != nil {
		slog.Warn("could not stop crane export", "error", err)
	}
	if _, err := microvmWaitStatus(exporter); err != nil {
		slog.Warn("could not wait for crane export", "error", err)
	}
}

// MicrovmImagePrune removes images nothing refers to, and scratch left by
// interrupted pulls. An image is in use if a reference names it, a VM was
// created from it, or a staged root replacement is about to use it.
func MicrovmImagePrune(layout MicrovmLayout) ([]string, error) {
	if err := layout.Ensure(); err != nil {
		return nil, err
	}
	var removed []string
	names, err := microvmReadDirNames(layout.Images())
	if err != nil {
		return nil, fmt.Errorf("list images: %w", microvmIOError(err))
	}
	for _, name := range names {
		if scratch, ok := strings.CutPrefix(name, ".tmp-"); ok {
			// A pull holds its image's lock while it writes scratch, and the
			// scratch name carries the id; waiting for the lock waits for it.
			id, _, _ := strings.Cut(scratch, "-")
			imageLock, err := layout.LockImage(id)
			if err != nil {
				continue
			}
			// Absent means the pull we waited for published it.
			err = os.RemoveAll(filepath.Join(layout.Images(), name))
			imageLock.Drop()
			if err != nil {
				return nil, fmt.Errorf("remove %s: %w", name, microvmIOError(err))
			}
		} else if len(name) == 64 {
			pruned, err := microvmImagePruneOne(layout, name)
			if err != nil {
				return nil, err
			}
			if pruned {
				removed = append(removed, name)
			}
		}
	}
	global, err := layout.LockGlobal()
	if err != nil {
		return nil, err
	}
	defer global.Drop()
	boot, err := MicrovmBootPrune(layout)
	if err != nil {
		return nil, err
	}
	return append(removed, boot...), nil
}

// microvmImagePruneOne removes image name unless it is in use. It decides
// under the image's lock, which pulls and runs hold from before they name
// the image until they have recorded their use of it. Lock order: image,
// then global.
func microvmImagePruneOne(layout MicrovmLayout, name string) (bool, error) {
	imageLock, err := layout.LockImage(name)
	if err != nil {
		return false, err
	}
	defer imageLock.Drop()
	global, err := layout.LockGlobal()
	if err != nil {
		return false, err
	}
	defer global.Drop()
	refs, err := MicrovmImageRefsRead(layout)
	if err != nil {
		return false, err
	}
	inUse := false
	for _, id := range refs.Refs {
		if id == name {
			inUse = true
		}
	}
	if !inUse {
		states, err := MicrovmVmStateList(layout)
		if err != nil {
			return false, err
		}
		for _, vm := range states {
			if vm.ImageID == name {
				inUse = true
			}
		}
	}
	if !inUse {
		pinned, err := MicrovmPinnedImages(layout)
		if err != nil {
			return false, err
		}
		inUse = pinned[name]
	}
	if inUse || !microvmExists(layout.Image(name)) {
		return false, nil
	}
	if err := os.RemoveAll(layout.Image(name)); err != nil {
		return false, fmt.Errorf("remove image %s: %w", name, microvmIOError(err))
	}
	if err := layout.ForgetImageLock(name, imageLock); err != nil {
		return false, err
	}
	return true, nil
}

// MicrovmRootImage is the path to an image's root disk.
func MicrovmRootImage(layout MicrovmLayout, id string) string {
	return filepath.Join(layout.Image(id), "root.ext4")
}
