// Port of mj-worker/src/move_workspace.rs.
//
// Move-only, disk-backed workspace capture. Never builds checkpoint
// payloads.
//
// Names that collide with other mj-worker modules carry this module's
// prefix: MoveWorkspaceInspect (microvm/vm's inspect kept the name while it
// was part of this crate) and MoveWorkspaceRestore (checkpoint/codex_goal's
// restore keeps it).

package worker

import (
	"archive/tar"
	"bytes"
	"crypto/sha256"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"log/slog"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"time"
	"unicode"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/checkpoint"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// manifestFileName is Rust's MANIFEST (renamed: capture's local manifest
// would shadow it).
const manifestFileName = "move-workspace.json"

func git(root string, args []string) ([]byte, error) {
	return gitWithInput(root, args, nil)
}

func gitWithInput(root string, args []string, input []byte) ([]byte, error) {
	output, err := core.RunWithInput(exec.Command("git", append([]string{"-C", root}, args...)...), input)
	if err != nil {
		return nil, err
	}
	if !output.Status.Success() {
		return nil, fmt.Errorf("git %s in %s: %s", strings.Join(args, " "), root, captureLossy(output.Stderr))
	}
	return output.Stdout, nil
}

func gitText(root string, args []string) (string, error) {
	output, err := git(root, args)
	if err != nil {
		return "", err
	}
	text, err := captureFromUTF8(output)
	if err != nil {
		return "", err
	}
	return strings.TrimRightFunc(text, unicode.IsSpace), nil
}

// gitPath is where Git keeps name in the Git directory of the checkout at
// root.
func gitPath(root string, name string) (string, error) {
	output, err := gitText(root, []string{"rev-parse", "--path-format=absolute", "--git-path", name})
	if err != nil {
		return "", err
	}
	return core.ResolveGitPath(root, output)
}

func gitFile(root string, args []string, destination string) error {
	file, err := os.Create(destination)
	if err != nil {
		return err
	}
	// Rust hands the file to the child and drops it when the child ends.
	defer file.Close()
	status, err := core.RunToFile(exec.Command("git", append([]string{"-C", root}, args...)...), file)
	if err != nil {
		return err
	}
	if !status.Success() {
		return fmt.Errorf("Move git %s failed in %s (%s)", strings.Join(args, " "), root, workFactsExitStatus(status))
	}
	return nil
}

// paths lists the untracked files Git does not ignore, or with ignored the
// ignored ones.
func paths(root string, ignored bool) ([]string, error) {
	args := []string{"ls-files", "--others", "--exclude-standard", "-z"}
	if ignored {
		args = append(args, "--ignored")
	}
	output, err := git(root, args)
	if err != nil {
		return nil, err
	}
	var listed []string
	for _, entry := range strings.Split(string(output), "\x00") {
		if entry == "" {
			continue
		}
		path, err := core.FromGitBytes([]byte(entry))
		if err != nil {
			return nil, err
		}
		listed = append(listed, path)
	}
	return listed, nil
}

func repositoryRoot(repository *core.WorkspaceRepository) (string, error) {
	if err := checkpoint.ValidateComponent(repository.ID, "Move repository"); err != nil {
		return "", err
	}
	return gitText(repository.Root, []string{"rev-parse", "--show-toplevel"})
}

// MoveWorkspaceInspect is Rust's move_workspace::inspect: what a Move would
// carry from each repository, with the space it needs.
func MoveWorkspaceInspect(repositories []core.WorkspaceRepository) (core.WorkspaceAssessment, error) {
	var assessment core.WorkspaceAssessment
	for index := range repositories {
		repository := &repositories[index]
		root, err := repositoryRoot(repository)
		if err != nil {
			return core.WorkspaceAssessment{}, err
		}
		unmerged, err := git(root, []string{"ls-files", "--unmerged", "-z"})
		if err != nil {
			return core.WorkspaceAssessment{}, err
		}
		if len(unmerged) != 0 {
			return core.WorkspaceAssessment{}, fmt.Errorf("resolve merge conflicts in %s before Move", repository.ID)
		}
		shared, err := gitText(root, []string{"rev-parse", "--shared-index-path"})
		if err != nil {
			return core.WorkspaceAssessment{}, err
		}
		if shared != "" {
			return core.WorkspaceAssessment{}, fmt.Errorf("%s uses a split Git index; run git update-index --no-split-index there before Move", repository.ID)
		}
		flagged, err := git(root, []string{"ls-files", "-v", "-z"})
		if err != nil {
			return core.WorkspaceAssessment{}, err
		}
		for _, entry := range bytes.Split(flagged, []byte{0}) {
			if len(entry) > 0 && (entry[0] == 'S' || ('a' <= entry[0] && entry[0] <= 'z')) {
				return core.WorkspaceAssessment{}, fmt.Errorf("%s has skip-worktree or assume-unchanged files; clear those flags before Move so all tracked edits can be verified", repository.ID)
			}
		}
		// Include alternate object stores and staged blobs, which can be
		// much larger than this checkout's own objects directory.
		historyText, err := gitText(root, []string{"rev-list", "--disk-usage", "--objects", "--all", "--reflog"})
		if err != nil {
			return core.WorkspaceAssessment{}, err
		}
		historyBytes, err := moveParseUint(historyText)
		if err != nil {
			return core.WorkspaceAssessment{}, fmt.Errorf("read Move Git history size: %w", err)
		}
		checkoutBytes := uint64(0)
		tree, err := git(root, []string{"ls-tree", "-r", "-l", "-z", "HEAD"})
		if err != nil {
			return core.WorkspaceAssessment{}, err
		}
		for _, line := range bytes.Split(tree, []byte{0}) {
			if len(line) == 0 {
				continue
			}
			header, _, _ := bytes.Cut(line, []byte{'\t'})
			if !utf8.Valid(header) {
				return core.WorkspaceAssessment{}, errors.New("invalid utf-8 sequence in a Git tree entry")
			}
			fields := strings.Fields(string(header))
			if len(fields) < 4 {
				return core.WorkspaceAssessment{}, errors.New("Git tree size missing")
			}
			if size := fields[3]; size != "-" {
				parsed, err := moveParseUint(size)
				if err != nil {
					return core.WorkspaceAssessment{}, err
				}
				checkoutBytes = moveSaturatingAdd(checkoutBytes, parsed)
			}
		}
		editsBytes := uint64(0)
		edited, err := git(root, []string{"diff", "--name-only", "-z", "HEAD"})
		if err != nil {
			return core.WorkspaceAssessment{}, err
		}
		for _, raw := range bytes.Split(edited, []byte{0}) {
			if len(raw) == 0 {
				continue
			}
			relative, err := core.FromGitBytes(raw)
			if err != nil {
				return core.WorkspaceAssessment{}, err
			}
			info, err := os.Lstat(filepath.Join(root, relative))
			switch {
			case err == nil:
				editsBytes = moveSaturatingAdd(editsBytes, uint64(info.Size()))
			case errors.Is(err, fs.ErrNotExist):
			default:
				return core.WorkspaceAssessment{}, err
			}
		}
		// Binary patch encoding and object repacking need overhead; this is
		// an estimate shown before the source is sealed, not a checkpoint
		// limit.
		assessment.RequiredBytes = moveSaturatingAdd(
			moveSaturatingAdd(moveSaturatingAdd(assessment.RequiredBytes, checkoutBytes), moveSaturatingMul(historyBytes, 2)),
			moveSaturatingMul(editsBytes, 2))
		untracked, err := paths(root, false)
		if err != nil {
			return core.WorkspaceAssessment{}, err
		}
		for _, path := range untracked {
			location := core.WorkspacePath{Repository: repository.ID, Path: path}
			if err := location.Validate(); err != nil {
				return core.WorkspaceAssessment{}, err
			}
			info, err := os.Lstat(filepath.Join(root, location.Path))
			if err != nil {
				return core.WorkspaceAssessment{}, err
			}
			symlink := info.Mode()&fs.ModeSymlink != 0
			if !info.Mode().IsRegular() && !symlink {
				return core.WorkspaceAssessment{}, fmt.Errorf("unsupported Move file %s", location.Path)
			}
			if symlink {
				target, err := os.Readlink(filepath.Join(root, location.Path))
				if err != nil {
					return core.WorkspaceAssessment{}, err
				}
				if err := checkpoint.ValidateSymlinkTarget(location.Path, target); err != nil {
					return core.WorkspaceAssessment{}, err
				}
			}
			assessment.Files = append(assessment.Files, core.WorkspaceFile{Location: location, Bytes: uint64(info.Size())})
		}
		ignored, err := paths(root, true)
		if err != nil {
			return core.WorkspaceAssessment{}, err
		}
		for _, path := range ignored {
			info, err := os.Lstat(filepath.Join(root, path))
			if err != nil {
				return core.WorkspaceAssessment{}, err
			}
			assessment.IgnoredFiles++
			assessment.IgnoredBytes = moveSaturatingAdd(assessment.IgnoredBytes, uint64(info.Size()))
		}
	}
	slices.SortStableFunc(assessment.Files, func(a, b core.WorkspaceFile) int { return a.Location.Compare(b.Location) })
	assessment.Roots = core.FileTree(&assessment)
	assessment.InitiallyExpanded = core.InitialExpansion(assessment.Roots)
	return assessment, nil
}

// moveParseUint is Rust's str::parse::<u64>, which accepts a leading `+`.
func moveParseUint(text string) (uint64, error) {
	value, err := strconv.ParseUint(strings.TrimPrefix(text, "+"), 10, 64)
	if err != nil {
		return 0, fmt.Errorf("invalid digit found in string: %q", text)
	}
	return value, nil
}

// moveSaturatingAdd and moveSaturatingMul are Rust's u64 saturating
// arithmetic.
func moveSaturatingAdd(a, b uint64) uint64 {
	if a+b < a {
		return ^uint64(0)
	}
	return a + b
}

func moveSaturatingMul(a, b uint64) uint64 {
	if a != 0 && b > ^uint64(0)/a {
		return ^uint64(0)
	}
	return a * b
}

// digest is the size and lowercase hexadecimal SHA-256 of the file at path.
func digest(path string) (uint64, string, error) {
	input, err := os.Open(path)
	if err != nil {
		return 0, "", err
	}
	defer input.Close()
	hash := sha256.New()
	size, err := io.CopyBuffer(hash, input, make([]byte, 128*1024))
	if err != nil {
		return 0, "", err
	}
	return uint64(size), core.LowerHex(hash.Sum(nil)), nil
}

func addFile(manifest *core.WorkspaceManifest, root string, relative string) error {
	size, hash, err := digest(filepath.Join(root, relative))
	if err != nil {
		return err
	}
	manifest.Files = append(manifest.Files, core.TransferFile{Path: relative, Bytes: size, Sha256: hash})
	return nil
}

// Capture stages the selected workspace of each repository under
// destination, which a later Restore applies. A finished stage is verified
// and reused.
func Capture(repositories []core.WorkspaceRepository, selection *core.WorkspaceSelection, destination string) (core.WorkspaceManifest, error) {
	assessment, err := MoveWorkspaceInspect(repositories)
	if err != nil {
		return core.WorkspaceManifest{}, err
	}
	if err := selection.Validate(&assessment); err != nil {
		return core.WorkspaceManifest{}, err
	}
	parent := filepath.Dir(destination)
	if parent == destination {
		return core.WorkspaceManifest{}, errors.New("Move stage has no parent")
	}
	if err := os.MkdirAll(parent, 0o777); err != nil {
		return core.WorkspaceManifest{}, err
	}
	lockFile, err := os.OpenFile(moveWithExtension(destination, "move-lock"), os.O_CREATE|os.O_RDWR, 0o666)
	if err != nil {
		return core.WorkspaceManifest{}, err
	}
	defer lockFile.Close()
	if err := moveTryLock(lockFile); err != nil {
		return core.WorkspaceManifest{}, fmt.Errorf("another process owns this Move capture; retry after it finishes: %w", err)
	}
	lock := core.NewFileLock(lockFile)
	defer lock.Drop()
	if info, err := os.Stat(filepath.Join(destination, manifestFileName)); err == nil && info.Mode().IsRegular() {
		return Verify(destination)
	}
	if _, err := os.Stat(destination); err == nil {
		if err := os.RemoveAll(destination); err != nil {
			return core.WorkspaceManifest{}, err
		}
	}
	if err := os.MkdirAll(destination, 0o777); err != nil {
		return core.WorkspaceManifest{}, err
	}
	manifest := core.WorkspaceManifest{Version: 1, Repositories: []core.TransferredRepository{}, Files: []core.TransferFile{}}
	for index := range repositories {
		repository := &repositories[index]
		transferred, err := captureRepository(repository, &assessment, selection, destination, &manifest)
		if err != nil {
			return core.WorkspaceManifest{}, err
		}
		manifest.Repositories = append(manifest.Repositories, transferred)
	}
	temporaryManifest := filepath.Join(destination, "manifest.pending")
	encoded, err := moveMarshalJSON(manifest)
	if err != nil {
		return core.WorkspaceManifest{}, err
	}
	file, err := os.Create(temporaryManifest)
	if err != nil {
		return core.WorkspaceManifest{}, err
	}
	if _, err := file.Write(encoded); err != nil {
		_ = file.Close()
		return core.WorkspaceManifest{}, err
	}
	if err := file.Sync(); err != nil {
		_ = file.Close()
		return core.WorkspaceManifest{}, err
	}
	if err := file.Close(); err != nil {
		return core.WorkspaceManifest{}, err
	}
	if err := os.Rename(temporaryManifest, filepath.Join(destination, manifestFileName)); err != nil {
		return core.WorkspaceManifest{}, err
	}
	return manifest, nil
}

// captureRepository stages one repository of a Capture and adds its
// payloads to manifest.
func captureRepository(repository *core.WorkspaceRepository, assessment *core.WorkspaceAssessment, selection *core.WorkspaceSelection, destination string, manifest *core.WorkspaceManifest) (core.TransferredRepository, error) {
	root, err := repositoryRoot(repository)
	if err != nil {
		return core.TransferredRepository{}, err
	}
	relative := repository.ID
	stage := filepath.Join(destination, relative)
	if err := os.Mkdir(stage, 0o777); err != nil {
		return core.TransferredRepository{}, err
	}
	head, err := gitText(root, []string{"rev-parse", "HEAD"})
	if err != nil {
		return core.TransferredRepository{}, err
	}
	branchText, err := gitText(root, []string{"rev-parse", "--abbrev-ref", "HEAD"})
	if err != nil {
		return core.TransferredRepository{}, err
	}
	var branch *string
	if branchText != "HEAD" {
		branch = &branchText
	}
	statusArgs := []string{"status", "--porcelain=v1", "--untracked-files=no"}
	status, err := gitText(root, statusArgs)
	if err != nil {
		return core.TransferredRepository{}, err
	}
	refsText, err := gitText(root, []string{"for-each-ref", "--format=%(refname) %(objectname)"})
	if err != nil {
		return core.TransferredRepository{}, err
	}
	var all [][2]string
	for _, line := range captureLines(refsText) {
		name, hash, ok := strings.Cut(line, " ")
		if !ok {
			return core.TransferredRepository{}, errors.New("invalid Git ref")
		}
		all = append(all, [2]string{name, hash})
	}
	refs := [][2]string{}
	for _, ref := range all {
		// Old review baseline pins are not carried: nothing reads them.
		if !core.IsReviewBaselinePin(ref[0]) {
			refs = append(refs, ref)
		}
	}
	symbolicText, err := gitText(root, []string{"for-each-ref", "--format=%(refname) %(symref)"})
	if err != nil {
		return core.TransferredRepository{}, err
	}
	symbolicRefs := [][2]string{}
	for _, line := range captureLines(symbolicText) {
		if name, target, ok := strings.Cut(line, " "); ok && target != "" {
			symbolicRefs = append(symbolicRefs, [2]string{name, target})
		}
	}
	stashPath, err := gitPath(root, "logs/refs/stash")
	if err != nil {
		return core.TransferredRepository{}, err
	}
	var stashLog *string
	switch log, err := os.ReadFile(stashPath); {
	case err == nil:
		if !utf8.Valid(log) {
			return core.TransferredRepository{}, errors.New("stream did not contain valid UTF-8")
		}
		text := string(log)
		stashLog = &text
	case errors.Is(err, fs.ErrNotExist):
	default:
		return core.TransferredRepository{}, err
	}
	bundle := filepath.Join(stage, "history.bundle")
	if !utf8.ValidString(bundle) {
		return core.TransferredRepository{}, errors.New("non-UTF8 Move stage")
	}
	bundleArgs := []string{"bundle", "create", bundle, "--all", "HEAD"}
	if stashLog != nil {
		for _, line := range captureLines(*stashLog) {
			if fields := strings.Fields(line); len(fields) > 1 {
				bundleArgs = append(bundleArgs, fields[1])
			}
		}
	}
	// A force push/reset can leave the immutable launch base unreachable
	// from every ref, but the next checkpoint still needs that object.
	managedBase := repository.BaseCommit
	if managedBase == nil {
		if managedBase, err = checkpoint.RemoteWorkspaceBase(checkpoint.SystemGit{}, root); err != nil {
			return core.TransferredRepository{}, err
		}
	}
	if managedBase != nil {
		available, err := checkpoint.ManagedBaseIsAvailable(checkpoint.SystemGit{}, root, *managedBase)
		if err != nil {
			return core.TransferredRepository{}, err
		}
		if !available {
			slog.Warn("legacy managed base object is absent; Move captures available history without an explicit base",
				"repository", repository.ID, "base_commit", *managedBase)
			managedBase = nil
		}
	}
	if managedBase != nil {
		bundleArgs = append(bundleArgs, *managedBase)
	}
	if err := gitFile(root, bundleArgs, filepath.Join(stage, "git-output")); err != nil {
		return core.TransferredRepository{}, err
	}
	if err := os.Remove(filepath.Join(stage, "git-output")); err != nil {
		return core.TransferredRepository{}, err
	}
	stagedArgs := []string{"diff", "--binary", "--full-index", "--cached", "HEAD"}
	unstagedArgs := []string{"diff", "--binary", "--full-index"}
	if err := gitFile(root, stagedArgs, filepath.Join(stage, "staged.patch")); err != nil {
		return core.TransferredRepository{}, err
	}
	if err := gitFile(root, unstagedArgs, filepath.Join(stage, "unstaged.patch")); err != nil {
		return core.TransferredRepository{}, err
	}
	index, err := gitPath(root, "index")
	if err != nil {
		return core.TransferredRepository{}, err
	}
	if err := moveCopyFile(index, filepath.Join(stage, "index")); err != nil {
		return core.TransferredRepository{}, err
	}
	for _, name := range []string{"history.bundle", "staged.patch", "unstaged.patch", "index"} {
		if err := addFile(manifest, destination, filepath.Join(relative, name)); err != nil {
			return core.TransferredRepository{}, err
		}
	}
	untracked := []core.WorkspacePath{}
	for _, file := range assessment.Files {
		if file.Location.Repository != repository.ID || !selection.Includes(&file.Location) {
			continue
		}
		input := filepath.Join(root, file.Location.Path)
		before, err := os.Lstat(input)
		if err != nil {
			return core.TransferredRepository{}, err
		}
		tarRelative := filepath.Join(relative, "untracked", fmt.Sprintf("%08d.tar", len(untracked)))
		tarPath := filepath.Join(destination, tarRelative)
		if err := os.MkdirAll(filepath.Dir(tarPath), 0o777); err != nil {
			return core.TransferredRepository{}, err
		}
		// Tar carries symlinks and permission bits with bounded memory. Each
		// file gets its own tar so retries transfer independent files.
		if err := moveWriteTar(tarPath, input, file.Location.Path); err != nil {
			return core.TransferredRepository{}, err
		}
		after, err := os.Lstat(input)
		if err != nil {
			return core.TransferredRepository{}, err
		}
		if before.Size() != after.Size() || !before.ModTime().Equal(after.ModTime()) {
			return core.TransferredRepository{}, fmt.Errorf("%s changed during Move capture; source retained", input)
		}
		if err := addFile(manifest, destination, tarRelative); err != nil {
			return core.TransferredRepository{}, err
		}
		untracked = append(untracked, file.Location)
	}
	for _, check := range []struct {
		name string
		args []string
	}{{"staged.patch", stagedArgs}, {"unstaged.patch", unstagedArgs}} {
		verification := filepath.Join(stage, "verify.patch")
		if err := gitFile(root, check.args, verification); err != nil {
			return core.TransferredRepository{}, err
		}
		verifiedBytes, verifiedHash, err := digest(verification)
		if err != nil {
			return core.TransferredRepository{}, err
		}
		stagedBytes, stagedHash, err := digest(filepath.Join(stage, check.name))
		if err != nil {
			return core.TransferredRepository{}, err
		}
		if verifiedBytes != stagedBytes || verifiedHash != stagedHash {
			return core.TransferredRepository{}, errors.New("tracked edits changed during Move capture; source retained")
		}
		if err := os.Remove(verification); err != nil {
			return core.TransferredRepository{}, err
		}
	}
	unchanged := false
	if current, err := gitText(root, []string{"rev-parse", "HEAD"}); err != nil {
		return core.TransferredRepository{}, err
	} else if current == head {
		currentStatus, err := gitText(root, statusArgs)
		if err != nil {
			return core.TransferredRepository{}, err
		}
		unchanged = currentStatus == status
	}
	if !unchanged {
		return core.TransferredRepository{}, fmt.Errorf("repository %s changed during Move capture; source retained", repository.ID)
	}
	return core.TransferredRepository{
		ID:           repository.ID,
		Head:         head,
		Branch:       branch,
		Refs:         refs,
		SymbolicRefs: symbolicRefs,
		StashLog:     stashLog,
		Status:       status,
		Untracked:    untracked,
	}, nil
}

// moveWithExtension is Rust's Path::with_extension: the last component's
// extension replaced by (or, without one, extended with) extension.
func moveWithExtension(path, extension string) string {
	directory, name := filepath.Split(path)
	stem := name
	if dot := strings.LastIndexByte(name, '.'); dot > 0 {
		stem = name[:dot]
	}
	return directory + stem + "." + extension
}

// moveCopyFile is Rust's fs::copy: the contents of source written over
// destination, which takes source's permissions.
func moveCopyFile(source, destination string) error {
	input, err := os.Open(source)
	if err != nil {
		return err
	}
	defer input.Close()
	info, err := input.Stat()
	if err != nil {
		return err
	}
	output, err := os.OpenFile(destination, os.O_WRONLY|os.O_CREATE|os.O_TRUNC, info.Mode().Perm())
	if err != nil {
		return err
	}
	if _, err := io.Copy(output, input); err != nil {
		_ = output.Close()
		return err
	}
	if err := output.Chmod(info.Mode().Perm()); err != nil {
		_ = output.Close()
		return err
	}
	return output.Close()
}

// moveWriteTar archives the file or symlink at input, named name, as the
// only entry of a new tar at path, the way Rust's tar::Builder does with
// follow_symlinks(false): a GNU header with the file's mode, owner and
// modification time.
func moveWriteTar(path, input, name string) error {
	info, err := os.Lstat(input)
	if err != nil {
		return err
	}
	link := ""
	if info.Mode()&fs.ModeSymlink != 0 {
		if link, err = os.Readlink(input); err != nil {
			return err
		}
	}
	header, err := tar.FileInfoHeader(info, link)
	if err != nil {
		return err
	}
	header.Name = filepath.ToSlash(name)
	header.Format = tar.FormatGNU
	header.ModTime = info.ModTime().Truncate(time.Second)
	header.AccessTime, header.ChangeTime = time.Time{}, time.Time{}
	header.Uname, header.Gname = "", ""
	file, err := os.Create(path)
	if err != nil {
		return err
	}
	defer file.Close()
	archive := tar.NewWriter(file)
	if err := archive.WriteHeader(header); err != nil {
		return err
	}
	if info.Mode().IsRegular() {
		content, err := os.Open(input)
		if err != nil {
			return err
		}
		_, err = io.Copy(archive, content)
		_ = content.Close()
		if err != nil {
			return err
		}
	}
	if err := archive.Close(); err != nil {
		return err
	}
	return file.Close()
}

// moveMarshalJSON encodes v as serde_json::to_writer does: compact, without
// Go's HTML escaping and without a trailing newline.
func moveMarshalJSON(v any) ([]byte, error) {
	var encoded bytes.Buffer
	encoder := json.NewEncoder(&encoded)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(v); err != nil {
		return nil, err
	}
	return bytes.TrimSuffix(encoded.Bytes(), []byte("\n")), nil
}

// moveComponentKey is a relative path keyed by its components, so paths
// compare as Rust's PathBuf does.
func moveComponentKey(path string) string {
	return strings.Join(capturePathComponents(path), "\x00")
}

// Verify checks a staged workspace against its manifest and returns the
// manifest.
func Verify(source string) (core.WorkspaceManifest, error) {
	encoded, err := os.ReadFile(filepath.Join(source, manifestFileName))
	if err != nil {
		return core.WorkspaceManifest{}, err
	}
	var manifest core.WorkspaceManifest
	if err := json.Unmarshal(encoded, &manifest); err != nil {
		return core.WorkspaceManifest{}, err
	}
	if manifest.Version != 1 {
		return core.WorkspaceManifest{}, fmt.Errorf("unsupported Move workspace version %d", manifest.Version)
	}
	expected := map[string]struct{}{}
	repositoryIDs := map[string]struct{}{}
	validOid := func(value string) bool {
		if len(value) != 40 && len(value) != 64 {
			return false
		}
		for index := 0; index < len(value); index++ {
			c := value[index]
			if !('0' <= c && c <= '9' || 'a' <= c && c <= 'f' || 'A' <= c && c <= 'F') {
				return false
			}
		}
		return true
	}
	for _, repository := range manifest.Repositories {
		if _, seen := repositoryIDs[repository.ID]; seen {
			return core.WorkspaceManifest{}, errors.New("duplicate Move repository")
		}
		repositoryIDs[repository.ID] = struct{}{}
		for _, name := range []string{"history.bundle", "staged.patch", "unstaged.patch", "index"} {
			expected[moveComponentKey(filepath.Join(repository.ID, name))] = struct{}{}
		}
		for index := range repository.Untracked {
			expected[moveComponentKey(filepath.Join(repository.ID, "untracked", fmt.Sprintf("%08d.tar", index)))] = struct{}{}
		}
		if err := checkpoint.ValidateComponent(repository.ID, "Move repository"); err != nil {
			return core.WorkspaceManifest{}, err
		}
		valid := validOid(repository.Head)
		for _, ref := range repository.Refs {
			if !strings.HasPrefix(ref[0], "refs/") || strings.Contains(ref[0], "\x00") || !validOid(ref[1]) {
				valid = false
			}
		}
		if !valid {
			return core.WorkspaceManifest{}, errors.New("invalid Move Git identities")
		}
		seen := map[string]struct{}{}
		for _, path := range repository.Untracked {
			key := moveComponentKey(path.Path)
			if _, repeated := seen[key]; path.Repository != repository.ID || repeated {
				return core.WorkspaceManifest{}, errors.New("invalid Move untracked inventory")
			}
			seen[key] = struct{}{}
			if err := path.Validate(); err != nil {
				return core.WorkspaceManifest{}, err
			}
		}
	}
	for _, file := range manifest.Files {
		key := moveComponentKey(file.Path)
		if _, ok := expected[key]; !ok {
			return core.WorkspaceManifest{}, errors.New("unexpected or duplicate Move payload")
		}
		delete(expected, key)
		prefix := source
		for _, component := range capturePathComponents(file.Path) {
			prefix = filepath.Join(prefix, component)
			info, err := os.Lstat(prefix)
			if err != nil {
				return core.WorkspaceManifest{}, err
			}
			if info.Mode()&fs.ModeSymlink != 0 {
				return core.WorkspaceManifest{}, errors.New("Move payload traverses a symlink")
			}
		}
		transfer := core.WorkspacePath{Repository: "transfer", Path: file.Path}
		if err := transfer.Validate(); err != nil {
			return core.WorkspaceManifest{}, err
		}
		size, hash, err := digest(filepath.Join(source, file.Path))
		if err != nil {
			return core.WorkspaceManifest{}, err
		}
		if size != file.Bytes || hash != file.Sha256 {
			return core.WorkspaceManifest{}, fmt.Errorf("Move transfer verification failed for %s", file.Path)
		}
	}
	if len(expected) > 0 {
		return core.WorkspaceManifest{}, errors.New("Move manifest is missing required payloads")
	}
	return manifest, nil
}

// MoveWorkspaceRestore is Rust's move_workspace::restore: it applies the
// staged workspace at source to the destination repositories.
func MoveWorkspaceRestore(source string, repositories []core.WorkspaceRepository) error {
	manifest, err := Verify(source)
	if err != nil {
		return err
	}
	for index := range manifest.Repositories {
		repository := &manifest.Repositories[index]
		position := slices.IndexFunc(repositories, func(candidate core.WorkspaceRepository) bool {
			return candidate.ID == repository.ID
		})
		if position < 0 {
			return errors.New("Move destination repository missing")
		}
		root, err := repositoryRoot(&repositories[position])
		if err != nil {
			return err
		}
		if err := restoreRepository(source, &manifest, repository, root); err != nil {
			return err
		}
	}
	return nil
}

// restoreRepository applies one staged repository of manifest to root.
func restoreRepository(source string, manifest *core.WorkspaceManifest, repository *core.TransferredRepository, root string) error {
	stage := filepath.Join(source, repository.ID)
	if info, err := os.Stat(root); err != nil || !info.IsDir() {
		return fmt.Errorf("Move destination %s is missing", root)
	}
	bundle := filepath.Join(stage, "history.bundle")
	if !utf8.ValidString(bundle) {
		return errors.New("non-UTF8 Move bundle")
	}
	if _, err := git(root, []string{"bundle", "unbundle", bundle}); err != nil {
		return err
	}
	if _, err := git(root, []string{"checkout", "--detach", "--force", repository.Head}); err != nil {
		return err
	}
	// Transfer keeps base objects, but only managed finalization owns the
	// base pin. Raw destinations must not acquire it. Legacy manifests may
	// also carry obsolete review baseline pins.
	var wantedRefs [][2]string
	savedRefs := map[string]struct{}{}
	for _, ref := range repository.Refs {
		if !core.IsReviewBaselinePin(ref[0]) && ref[0] != core.HistoryCutoffBaseRef {
			wantedRefs = append(wantedRefs, ref)
			savedRefs[ref[0]] = struct{}{}
		}
	}
	existing, err := gitText(root, []string{"for-each-ref", "--format=%(refname)"})
	if err != nil {
		return err
	}
	var deletions []byte
	for _, line := range captureLines(existing) {
		// Existing pins belong to the destination, potentially shared with
		// other linked worktrees. Move neither installs its base pin nor
		// removes a pin it did not create; doctor reports raw residue.
		residue := core.IsReviewBaselinePin(line) || strings.HasPrefix(line, "refs/hel/") || line == core.HistoryCutoffBaseRef
		if _, saved := savedRefs[line]; !saved && !residue {
			deletions = fmt.Appendf(deletions, "delete %s\x00\x00", line)
		}
	}
	// Delete first: Git cannot replace a ref with its child in one
	// transaction.
	if len(deletions) > 0 {
		if _, err := gitWithInput(root, []string{"update-ref", "--no-deref", "--stdin", "-z"}, deletions); err != nil {
			return err
		}
	}
	var updates []byte
	for _, ref := range wantedRefs {
		updates = fmt.Appendf(updates, "update %s\x00%s\x00\x00", ref[0], ref[1])
	}
	if len(updates) > 0 {
		if _, err := gitWithInput(root, []string{"update-ref", "--no-deref", "--stdin", "-z"}, updates); err != nil {
			return err
		}
	}
	for _, symbolic := range repository.SymbolicRefs {
		name, target := symbolic[0], symbolic[1]
		if name == core.HistoryCutoffBaseRef {
			continue
		}
		if !strings.HasPrefix(name, "refs/") || !strings.HasPrefix(target, "refs/") {
			return errors.New("invalid symbolic Move ref")
		}
		if _, err := git(root, []string{"symbolic-ref", name, target}); err != nil {
			return err
		}
	}
	if repository.Branch != nil {
		if _, err := git(root, []string{"checkout", "--no-guess", *repository.Branch}); err != nil {
			return err
		}
	}
	for _, patch := range []struct {
		name  string
		index bool
	}{{"staged.patch", true}, {"unstaged.patch", false}} {
		path := filepath.Join(stage, patch.name)
		info, err := os.Stat(path)
		if err != nil {
			return err
		}
		if info.Size() == 0 {
			continue
		}
		args := []string{"apply", "--binary", "--whitespace=nowarn"}
		if patch.index {
			args = append(args, "--index")
		}
		if !utf8.ValidString(path) {
			return errors.New("non-UTF8 Move patch")
		}
		if _, err := git(root, append(args, path)); err != nil {
			return err
		}
	}
	index, err := gitPath(root, "index")
	if err != nil {
		return err
	}
	if err := moveCopyFile(filepath.Join(stage, "index"), index); err != nil {
		return err
	}
	if repository.StashLog != nil {
		path, err := gitPath(root, "logs/refs/stash")
		if err != nil {
			return err
		}
		if err := os.MkdirAll(filepath.Dir(path), 0o777); err != nil {
			return err
		}
		if err := os.WriteFile(path, []byte(*repository.StashLog), 0o666); err != nil {
			return err
		}
	}
	untrackedPrefix := filepath.Join(repository.ID, "untracked")
	for _, file := range manifest.Files {
		if !capturePathStartsWith(file.Path, untrackedPrefix) {
			continue
		}
		if err := restoreUntracked(filepath.Join(source, file.Path), repository, root); err != nil {
			return err
		}
	}
	head, err := gitText(root, []string{"rev-parse", "HEAD"})
	if err != nil {
		return err
	}
	if head != repository.Head {
		return errors.New("Move HEAD verification failed")
	}
	status, err := gitText(root, []string{"status", "--porcelain=v1", "--untracked-files=no"})
	if err != nil {
		return err
	}
	if status != repository.Status {
		return errors.New("Move index/worktree verification failed")
	}
	return nil
}

// restoreUntracked unpacks the one untracked file a staged tar holds into
// root.
func restoreUntracked(archivePath string, repository *core.TransferredRepository, root string) error {
	file, err := os.Open(archivePath)
	if err != nil {
		return err
	}
	defer file.Close()
	archive := tar.NewReader(file)
	count := 0
	for {
		header, err := archive.Next()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return err
		}
		count++
		path := header.Name
		listed := slices.ContainsFunc(repository.Untracked, func(candidate core.WorkspacePath) bool {
			return moveComponentKey(candidate.Path) == moveComponentKey(path)
		})
		if !listed {
			return fmt.Errorf("unlisted Move file %s", path)
		}
		symlink := header.Typeflag == tar.TypeSymlink
		if symlink {
			if header.Linkname == "" {
				return errors.New("Move symlink target missing")
			}
			if err := checkpoint.ValidateSymlinkTarget(path, header.Linkname); err != nil {
				return err
			}
		}
		regular := header.Typeflag == tar.TypeReg || header.Typeflag == tar.TypeRegA
		if !regular && header.Typeflag != tar.TypeGNUSparse && !symlink {
			return errors.New("unsupported Move archive entry")
		}
		parent := root
		components := capturePathComponents(path)
		if len(components) > 1 {
			for _, component := range components[:len(components)-1] {
				parent = filepath.Join(parent, component)
				info, err := os.Lstat(parent)
				switch {
				case err == nil:
					if !info.IsDir() {
						return errors.New("Move destination parent is not a directory")
					}
				case errors.Is(err, fs.ErrNotExist):
				default:
					return err
				}
			}
		}
		unpacked, err := moveUnpackIn(root, header, archive)
		if err != nil {
			return err
		}
		if !unpacked {
			return fmt.Errorf("unsafe Move file path %s", path)
		}
	}
	if count != 1 {
		return errors.New("Move file payload must have exactly one entry")
	}
	return nil
}

// moveUnpackIn is Rust's tar::Entry::unpack_in: it unpacks one entry inside
// root, refusing (false) a path with a `..` component, creating missing
// parents, and refusing a parent that resolves outside root. A regular
// file takes the entry's permission bits and modification time; an
// existing file or link at the path is replaced.
func moveUnpackIn(root string, header *tar.Header, content io.Reader) (bool, error) {
	var parts []string
	for _, part := range strings.Split(header.Name, "/") {
		switch part {
		case "", ".":
		case "..":
			return false, nil
		default:
			parts = append(parts, part)
		}
	}
	if len(parts) == 0 {
		return true, nil
	}
	destination := filepath.Join(append([]string{root}, parts...)...)
	parent := filepath.Dir(destination)
	if err := os.MkdirAll(parent, 0o777); err != nil {
		return false, fmt.Errorf("failed to create `%s`: %w", parent, err)
	}
	canonicalParent, err := hookCanonicalize(parent)
	if err != nil {
		return false, err
	}
	canonicalRoot, err := hookCanonicalize(root)
	if err != nil {
		return false, err
	}
	if !capturePathStartsWith(canonicalParent, canonicalRoot) {
		return false, fmt.Errorf("trying to unpack outside of destination path: %s", canonicalRoot)
	}
	if header.Typeflag == tar.TypeSymlink {
		err := os.Symlink(header.Linkname, destination)
		if errors.Is(err, fs.ErrExist) {
			if err = os.Remove(destination); err == nil {
				err = os.Symlink(header.Linkname, destination)
			}
		}
		if err != nil {
			return false, fmt.Errorf("failed to unpack `%s`: %w", destination, err)
		}
		return true, nil
	}
	file, err := os.OpenFile(destination, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o666)
	if errors.Is(err, fs.ErrExist) {
		if err = os.Remove(destination); err == nil {
			file, err = os.OpenFile(destination, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o666)
		}
	}
	if err != nil {
		return false, fmt.Errorf("failed to unpack `%s`: %w", destination, err)
	}
	if _, err := io.Copy(file, content); err != nil {
		_ = file.Close()
		return false, fmt.Errorf("failed to unpack `%s`: %w", destination, err)
	}
	if err := file.Chmod(fs.FileMode(header.Mode & 0o777)); err != nil {
		_ = file.Close()
		return false, err
	}
	if err := file.Close(); err != nil {
		return false, err
	}
	modified := header.ModTime
	if modified.Unix() == 0 {
		modified = time.Unix(1, 0)
	}
	if err := os.Chtimes(destination, modified, modified); err != nil {
		return false, err
	}
	return true, nil
}

// Execute runs one workspace command and returns its JSON answer
// (serde_json::to_value of the result; null for a restore).
func Execute(command core.WorkspaceCommand) (json.RawMessage, error) {
	var result any
	var err error
	switch command := command.(type) {
	case core.WorkspaceCommandInspect:
		result, err = MoveWorkspaceInspect(command.Repositories)
	case core.WorkspaceCommandCapture:
		result, err = Capture(command.Repositories, &command.Selection, command.Destination)
	case core.WorkspaceCommandRestore:
		err = MoveWorkspaceRestore(command.Source, command.Repositories)
	case core.WorkspaceCommandVerify:
		result, err = Verify(command.Source)
	default:
		return nil, errors.New("unknown Move workspace command")
	}
	if err != nil {
		return nil, err
	}
	return moveMarshalJSON(result)
}
