// Port of mj-core/src/config/dsh_credentials.rs.
//
// DSH's own credential store, read the way DeepSeek Harness reads it.
//
// DSH keeps the API key a person saves in it under `refs` in
// `<DSH home>/.credentials.yaml` (`@deepseek-ai/dsh-credentials-local`), and
// reads `<DSH home>/.env` as its lowest fallback. A DSH profile's home is that
// DSH home, so Mjolnir finds the same key DSH would use.
//
// A DeepSeek account sign-in is a different credential: a token DSH sends in
// `x-dsh-auth-token` to its Messages endpoint. dsh-acp speaks the
// OpenAI-compatible API with a bearer key only, so the sign-in is recognized
// and reported, never sent.

package core

import (
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"unicode"

	"go.yaml.in/yaml/v3"
)

// dshCredentialsFile is the managed store, beside the `.env` fallback in the
// DSH home.
const dshCredentialsFile = ".credentials.yaml"

const dshDotenvFile = ".env"

// dshAccountRecord is the record DSH's browser sign-in writes
// (`deepseek-account-platform`).
const dshAccountRecord = "deepseek-account-platform/default"

// dshDocumentVersion is the only layout DSH reads, besides the flat one it
// migrates from.
const dshDocumentVersion int64 = 1

// dshStoredCredentials is what DSH's credential store holds for one key.
type dshStoredCredentials struct {
	// APIKey is the key saved under `refs`, when there is one.
	APIKey *string
	// AccountSignedIn is whether DSH is signed in to a DeepSeek account.
	AccountSignedIn bool
}

// dshStored reads name from the store in home. A missing file holds nothing;
// a file DSH itself would refuse to load is an error; the caller decides
// whether another source makes it harmless.
func dshStored(home, name string) (dshStoredCredentials, error) {
	path := filepath.Join(home, dshCredentialsFile)
	text, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		return dshStoredCredentials{}, nil
	}
	if err != nil {
		return dshStoredCredentials{}, fmt.Errorf("read DSH's credentials %s: %w", path, err)
	}
	// DSH refuses a credential file any other user can read.
	problem, found, err := SecretsPermissionProblem(path)
	if err != nil {
		return dshStoredCredentials{}, err
	}
	if found {
		return dshStoredCredentials{}, fmt.Errorf("%s, so DSH refuses it; run `chmod 600 %s`", problem, path)
	}
	stored, err := dshCredentialsParse(string(text), name)
	if err != nil {
		return dshStoredCredentials{}, fmt.Errorf("read DSH's credentials %s: %w", path, err)
	}
	return stored, nil
}

// yamlErrorLine finds the line number in a go-yaml error message.
var yamlErrorLine = regexp.MustCompile(`line (\d+)`)

// dshCredentialsParse is Rust's dsh_credentials::parse (prefixed: `parse`
// collides across modules).
func dshCredentialsParse(text, name string) (dshStoredCredentials, error) {
	// Messages name keys and positions only: every value here is a secret.
	var documents []*yaml.Node
	decoder := yaml.NewDecoder(strings.NewReader(text))
	for {
		var document yaml.Node
		err := decoder.Decode(&document)
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			line := 0
			if match := yamlErrorLine.FindStringSubmatch(err.Error()); match != nil {
				line, _ = strconv.Atoi(match[1])
			}
			return dshStoredCredentials{}, fmt.Errorf("not valid YAML at line %d", line)
		}
		documents = append(documents, &document)
	}
	var root *yaml.Node
	switch len(documents) {
	case 0:
		return dshStoredCredentials{}, nil
	case 1:
		root = yamlContent(documents[0])
	default:
		return dshStoredCredentials{}, errors.New("holds more than one YAML document")
	}
	if root == nil || yamlIsNull(root) {
		return dshStoredCredentials{}, nil
	}
	if root.Kind != yaml.MappingNode {
		return dshStoredCredentials{}, errors.New("its root is not a mapping")
	}
	if len(root.Content) == 0 {
		return dshStoredCredentials{}, nil
	}
	version := yamlField(root, "version")
	if version == nil {
		// The pre-release flat layout: refs at the top level. DSH migrates it
		// in place on its next start without changing a value.
		key, err := dshReference(root, name)
		if err != nil {
			return dshStoredCredentials{}, err
		}
		return dshStoredCredentials{APIKey: key}, nil
	}
	// DSH compares with `!==`, so `1.0` is version 1 too.
	isCurrent := false
	switch version.ShortTag() {
	case "!!int":
		var number int64
		isCurrent = version.Decode(&number) == nil && number == dshDocumentVersion
	case "!!float":
		var number float64
		isCurrent = version.Decode(&number) == nil && number == float64(dshDocumentVersion)
	}
	if !isCurrent {
		return dshStoredCredentials{}, fmt.Errorf(
			"declares a layout version other than %d, the one Mjolnir reads", dshDocumentVersion)
	}
	var stored dshStoredCredentials
	if refs := yamlField(root, "refs"); refs != nil && !yamlIsNull(refs) {
		key, err := dshReference(refs, name)
		if err != nil {
			return dshStoredCredentials{}, err
		}
		stored.APIKey = key
	}
	if records := yamlField(root, "records"); records != nil && !yamlIsNull(records) {
		if records.Kind != yaml.MappingNode {
			return dshStoredCredentials{}, errors.New("`records` is not a mapping")
		}
		stored.AccountSignedIn = yamlField(records, dshAccountRecord) != nil
	}
	return stored, nil
}

// dshReference reads name from a `refs` section.
func dshReference(section *yaml.Node, name string) (*string, error) {
	if section.Kind != yaml.MappingNode {
		return nil, errors.New("`refs` is not a mapping")
	}
	value := yamlField(section, name)
	if value == nil {
		return nil, nil
	}
	if value.Kind != yaml.ScalarNode || value.ShortTag() != "!!str" {
		return nil, fmt.Errorf("the value for `%s` must be a string", name)
	}
	if value.Value == "" {
		return nil, fmt.Errorf("the value for `%s` is empty; remove the key instead", name)
	}
	key := value.Value
	return &key, nil
}

// yamlContent is the node a document or alias stands for.
func yamlContent(node *yaml.Node) *yaml.Node {
	for node != nil {
		switch node.Kind {
		case yaml.DocumentNode:
			if len(node.Content) == 0 {
				return nil
			}
			node = node.Content[0]
		case yaml.AliasNode:
			node = node.Alias
		default:
			return node
		}
	}
	return nil
}

func yamlIsNull(node *yaml.Node) bool {
	return node.Kind == yaml.ScalarNode && node.ShortTag() == "!!null"
}

// yamlField is the value under the string key in mapping; the last one wins
// when the key repeats.
func yamlField(mapping *yaml.Node, key string) *yaml.Node {
	var found *yaml.Node
	for i := 0; i+1 < len(mapping.Content); i += 2 {
		candidate := yamlContent(mapping.Content[i])
		if candidate != nil && candidate.Kind == yaml.ScalarNode && candidate.ShortTag() == "!!str" &&
			candidate.Value == key {
			found = yamlContent(mapping.Content[i+1])
			if found == nil {
				found = &yaml.Node{Kind: yaml.ScalarNode, Tag: "!!null"}
			}
		}
	}
	return found
}

// dshDotenv is name from the `.env` in home, DSH's lowest-ranked layer. DSH
// loads it with Node's util.parseEnv; this reads the same syntax for one
// variable.
func dshDotenv(home, name string) (*string, error) {
	path := filepath.Join(home, dshDotenvFile)
	// Node decodes the file as UTF-8, replacing what is not.
	data, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, fmt.Errorf("read DSH's %s: %w", path, err)
	}
	return parseDotenv(strings.ToValidUTF8(string(data), "\uFFFD"), name), nil
}

// parseDotenv reads Node's dotenv grammar: `[export ]NAME=value` lines, `#`
// comments, and single, double or backtick quotes that may span lines; double
// quotes expand `\n`. A later assignment replaces an earlier one.
func parseDotenv(text, name string) *string {
	text = strings.ReplaceAll(text, "\r\n", "\n")
	rest := text
	var found *string
	for rest != "" {
		line, after, _ := strings.Cut(rest, "\n")
		rest = after
		line = strings.TrimLeftFunc(line, unicode.IsSpace)
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}
		key, value, ok := strings.Cut(line, "=")
		if !ok {
			continue
		}
		key = strings.TrimSpace(key)
		if stripped, ok := strings.CutPrefix(key, "export "); ok {
			key = strings.TrimLeftFunc(stripped, unicode.IsSpace)
		}
		value = strings.TrimLeftFunc(value, unicode.IsSpace)
		var parsed string
		if value != "" && (value[0] == '"' || value[0] == '\'' || value[0] == '`') {
			quote := value[:1]
			// The closing quote may be on a later line; the value then
			// continues through it.
			body := value[1:]
			var quoted *string
			if end := strings.Index(body, quote); end >= 0 {
				inner := body[:end]
				quoted = &inner
			} else if end := strings.Index(rest, quote); end >= 0 {
				inner := body + "\n" + rest[:end]
				quoted = &inner
				_, rest, _ = strings.Cut(rest[end+1:], "\n")
			}
			switch {
			// Only a closed double-quoted value expands `\n`.
			case quoted != nil && quote == `"`:
				parsed = strings.ReplaceAll(*quoted, `\n`, "\n")
			case quoted != nil:
				parsed = *quoted
			default:
				// An unclosed quote is read as written up to the line end.
				parsed = strings.TrimRightFunc(value, unicode.IsSpace)
			}
		} else {
			unquoted, _, _ := strings.Cut(value, "#")
			parsed = strings.TrimRightFunc(unquoted, unicode.IsSpace)
		}
		if key == name {
			found = &parsed
		}
	}
	return found
}
