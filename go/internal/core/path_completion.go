// Port of mj-core/src/path_completion.rs.

// Path completion primitives shared by every surface that edits a path.
//
// Completion is a text protocol: the caller passes the text a user typed and
// receives candidates in the same shape. Nothing here decides which host
// owns a path; the caller names the host and supplies the executor.

package core

import (
	"encoding/json"
	"errors"
	"fmt"
	"strings"
)

// MaxCandidates is the most candidates any completion returns. A longer list
// is not readable in a popup, and the caller reports that more matches
// exist.
const MaxCandidates = 50

// CompletionKind is what a path field accepts. The zero value is the Rust
// default, CompletionKindDirectories.
type CompletionKind int

const (
	// Only directories, each with a trailing separator.
	CompletionKindDirectories CompletionKind = iota
	// Directories and files. Directories keep the trailing separator.
	CompletionKindAny
)

func (k CompletionKind) MarshalText() ([]byte, error) {
	switch k {
	case CompletionKindDirectories:
		return []byte("directories"), nil
	case CompletionKindAny:
		return []byte("any"), nil
	}
	return nil, fmt.Errorf("invalid CompletionKind %d", int(k))
}

func (k *CompletionKind) UnmarshalText(text []byte) error {
	switch string(text) {
	case "directories":
		*k = CompletionKindDirectories
	case "any":
		*k = CompletionKindAny
	default:
		return fmt.Errorf("unknown variant `%s`, expected `directories` or `any`", text)
	}
	return nil
}

// CompletionHost is the machine whose filesystem a path belongs to. A sealed
// interface over CompletionHostLocal, CompletionHostTarget and
// CompletionHostMachine, serialized as serde's externally tagged enum with
// the Rust variant names.
type CompletionHost interface {
	isCompletionHost()
}

// CompletionHostLocal is the machine running the controller.
type CompletionHostLocal struct{}

// CompletionHostTarget is the machine behind a configured target template.
type CompletionHostTarget struct{ ID string }

// CompletionHostMachine is a configured machine, which owns a home directory
// of its own.
type CompletionHostMachine struct{ Machine Machine }

func (CompletionHostLocal) isCompletionHost()   {}
func (CompletionHostTarget) isCompletionHost()  {}
func (CompletionHostMachine) isCompletionHost() {}

func (CompletionHostLocal) MarshalJSON() ([]byte, error) { return []byte(`"Local"`), nil }

func (h CompletionHostTarget) MarshalJSON() ([]byte, error) {
	return json.Marshal(map[string]string{"Target": h.ID})
}

func (h CompletionHostMachine) MarshalJSON() ([]byte, error) {
	return json.Marshal(map[string]Machine{"Machine": h.Machine})
}

// unmarshalCompletionHost decodes serde's externally tagged form.
func unmarshalCompletionHost(data []byte) (CompletionHost, error) {
	var unit string
	if err := json.Unmarshal(data, &unit); err == nil {
		if unit == "Local" {
			return CompletionHostLocal{}, nil
		}
		return nil, fmt.Errorf("unknown variant `%s`, expected one of `Local`, `Target`, `Machine`", unit)
	}
	var tagged map[string]json.RawMessage
	if err := json.Unmarshal(data, &tagged); err != nil {
		return nil, err
	}
	if len(tagged) != 1 {
		return nil, errors.New("invalid type: expected a CompletionHost map with a single key")
	}
	for variant, content := range tagged {
		switch variant {
		case "Target":
			var id string
			if err := json.Unmarshal(content, &id); err != nil {
				return nil, err
			}
			return CompletionHostTarget{ID: id}, nil
		case "Machine":
			machine, err := unmarshalMachine(content)
			if err != nil {
				return nil, err
			}
			return CompletionHostMachine{Machine: machine}, nil
		case "Local":
			return nil, errors.New("invalid type: unit variant `Local` given content")
		default:
			return nil, fmt.Errorf("unknown variant `%s`, expected one of `Local`, `Target`, `Machine`", variant)
		}
	}
	panic("unreachable")
}

// PathCompletion is one answer to a completion request.
type PathCompletion struct {
	Candidates []string `json:"candidates"`
	// The single match, or the longer shared prefix a client may insert.
	Insert *string `json:"insert"`
	// Whether matches were dropped to reach MaxCandidates.
	Truncated bool `json:"truncated"`
}

func (c *PathCompletion) UnmarshalJSON(data []byte) error {
	type wire PathCompletion
	return unmarshalDenyUnknown(data, (*wire)(c))
}

// LooksLikePath is whether text asks to be read as a filesystem path rather
// than a URL or an `owner/repo` shorthand. The same predicate decides it in
// remote_git.
func LooksLikePath(text string) bool {
	return strings.HasPrefix(text, "/") ||
		strings.HasPrefix(text, "~") ||
		strings.HasPrefix(text, ".") ||
		isWindowsAbsolutePath(text)
}

// LocalCompletions completes an on-disk path without spawning a shell.
func LocalCompletions(prefix string, kind CompletionKind) []string {
	panic("unported: mj-core/src/path_completion.rs local_completions")
}

// SSHCompletions completes a path through the configured SSH target.
//
// The SSH connection timeout and noninteractive mode keep a completion
// request from blocking a screen when a host is unavailable. The quoted
// prefix remains literal while the trailing glob is expanded only by the
// remote shell.
func SSHCompletions(ssh *SshTarget, prefix string, kind CompletionKind, executor CommandExecutor) ([]string, error) {
	panic("unported: mj-core/src/path_completion.rs ssh_completions")
}

// extendsPrefixByOneComponent is whether a remote line is a candidate: it
// extends the prefix by exactly one component, and the remote encoding
// survived the round trip.
func extendsPrefixByOneComponent(path string, prefix string, kind CompletionKind) bool {
	remainder, ok := strings.CutPrefix(path, prefix)
	if !ok || remainder == "" || strings.ContainsRune(path, '�') {
		return false
	}
	directory := strings.HasSuffix(path, "/")
	if kind == CompletionKindDirectories && !directory {
		return false
	}
	body := remainder
	if directory {
		body = remainder[:len(remainder)-1]
	}
	return !strings.Contains(body, "/")
}

// CommonInsert returns the single match or the extra shared path prefix
// that a client can insert without choosing between candidates.
func CommonInsert(prefix string, candidates []string) *string {
	if len(candidates) == 0 {
		return nil
	}
	first := candidates[0]
	if len(candidates) == 1 {
		return &first
	}
	common := []rune(first)
	for _, next := range candidates[1:] {
		other := []rune(next)
		n := 0
		for n < len(common) && n < len(other) && common[n] == other[n] {
			n++
		}
		common = common[:n]
	}
	text := string(common)
	if len(text) > len(prefix) && strings.HasPrefix(text, prefix) {
		return &text
	}
	return nil
}
