// Port of mj-worker/src/move_workspace.rs.
//
// Move-only, disk-backed workspace capture. Never builds checkpoint
// payloads.
//
// Names that collide with other mj-worker modules carry this module's
// prefix: MoveWorkspaceInspect (microvm/vm's inspect keeps the name) and
// MoveWorkspaceRestore (checkpoint/codex_goal's restore keeps it).

package worker

import (
	"crypto/sha256"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"unicode"

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
	panic("unported: mj-worker/src/move_workspace.rs inspect")
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
	panic("unported: mj-worker/src/move_workspace.rs capture")
}

// Verify checks a staged workspace against its manifest and returns the
// manifest.
func Verify(source string) (core.WorkspaceManifest, error) {
	panic("unported: mj-worker/src/move_workspace.rs verify")
}

// MoveWorkspaceRestore is Rust's move_workspace::restore: it applies the
// staged workspace at source to the destination repositories.
func MoveWorkspaceRestore(source string, repositories []core.WorkspaceRepository) error {
	panic("unported: mj-worker/src/move_workspace.rs restore")
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
	return json.Marshal(result)
}
