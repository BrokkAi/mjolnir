// Port of mj-core/src/codex_provider.rs.

// Reads the selected model provider out of a Codex profile home.
//
// Codex keeps its configuration in `config.toml` inside `CODEX_HOME`. A
// `model_provider` can select one of Codex's built-in providers, or a custom
// provider described by `[model_providers.<id>]`. Mjolnir copies the file
// verbatim into the staged profile home; it reads only the provider identity
// and the custom-provider fields it needs for authentication and catalog
// discovery.
//
// A profile with no `config.toml`, or one that names no `model_provider`,
// uses Codex's default OpenAI provider and reports nil here.
//
// A profile may also name a Codex profile overlay: `<name>.config.toml` in
// the same home, which `codex --profile <name>` layers over `config.toml`.
// `codex app-server`, which the ACP bridge runs, refuses `--profile`, so
// Mjolnir does the layering itself. EffectiveConfig is the one place that
// does it; the provider readers here and the staged home both use it.

package core

import (
	"errors"
	"fmt"
	"io/fs"
	"net/url"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"github.com/BurntSushi/toml"
)

// CodexProvider is the selected Codex provider, represented according to
// who defines it.
type CodexProvider struct {
	Definition CodexProviderDefinition
	// ModelCatalogJSON is the top-level `model_catalog_json` path, when the
	// profile's Codex `config.toml` names one. A relative path resolves
	// against `CODEX_HOME`.
	ModelCatalogJSON *string
}

// CodexProviderDefinition is CodexProviderDefinitionBuiltIn or
// CodexProviderDefinitionCustom.
type CodexProviderDefinition interface {
	isCodexProviderDefinition()
}

// CodexProviderDefinitionBuiltIn is a provider Codex ships and configures
// itself. Its optional provider table is left to Codex, which owns the
// built-in provider schema.
type CodexProviderDefinitionBuiltIn struct {
	Provider BuiltInCodexProvider
}

// CodexProviderDefinitionCustom is a provider described by the profile's
// `[model_providers.<id>]` table.
type CodexProviderDefinitionCustom struct {
	Provider CustomCodexProvider
}

func (CodexProviderDefinitionBuiltIn) isCodexProviderDefinition() {}
func (CodexProviderDefinitionCustom) isCodexProviderDefinition()  {}

// BuiltInCodexProvider's values are the Codex provider ids.
type BuiltInCodexProvider string

const (
	BuiltInCodexProviderOpenAi               BuiltInCodexProvider = "openai"
	BuiltInCodexProviderAmazonBedrock        BuiltInCodexProvider = "amazon-bedrock"
	BuiltInCodexProviderAmazonBedrockRuntime BuiltInCodexProvider = "amazon-bedrock-runtime"
	BuiltInCodexProviderOllama               BuiltInCodexProvider = "ollama"
	BuiltInCodexProviderLmStudio             BuiltInCodexProvider = "lmstudio"
)

func BuiltInCodexProviderFromID(id string) (BuiltInCodexProvider, bool) {
	switch provider := BuiltInCodexProvider(id); provider {
	case BuiltInCodexProviderOpenAi, BuiltInCodexProviderAmazonBedrock, BuiltInCodexProviderAmazonBedrockRuntime,
		BuiltInCodexProviderOllama, BuiltInCodexProviderLmStudio:
		return provider, true
	}
	return "", false
}

func (p BuiltInCodexProvider) ID() string { return string(p) }

func (p BuiltInCodexProvider) UsesAWSCredentials() bool {
	return p == BuiltInCodexProviderAmazonBedrock || p == BuiltInCodexProviderAmazonBedrockRuntime
}

// UsesCodexLogin: only the built-in OpenAI provider uses Codex's own login
// file.
func (p BuiltInCodexProvider) UsesCodexLogin() bool { return p == BuiltInCodexProviderOpenAi }

// NeedsNoAuthentication: these local providers do not need a login file or
// an API key.
func (p BuiltInCodexProvider) NeedsNoAuthentication() bool {
	return p == BuiltInCodexProviderOllama || p == BuiltInCodexProviderLmStudio
}

// CustomCodexProvider's String and GoString are written by hand so an inline
// key is redacted.
type CustomCodexProvider struct {
	ID      string
	BaseURL string
	// EnvKey is the environment variable that carries the API key, when the
	// provider uses `env_key`.
	EnvKey *string
	// BearerToken is the key itself, when the provider inlines it as
	// `experimental_bearer_token`. It already lives in the profile's
	// `config.toml`, which Mjolnir copies into every staged home, so Mjolnir
	// may use it for its own provider calls but never treats it as a secret
	// of its own.
	BearerToken *string
}

// CodexProviderKey is a custom provider's API key source, as its own table
// names it: CodexProviderKeyEnvKey or CodexProviderKeyInline.
//
// ParseConfig rejects a table that names neither, so a custom provider
// always resolves to one of these.
type CodexProviderKey interface {
	isCodexProviderKey()
}

// CodexProviderKeyEnvKey: the profile supplies the key in this environment
// variable.
type CodexProviderKeyEnvKey string

// CodexProviderKeyInline: the key is written into the provider table itself.
type CodexProviderKeyInline string

func (CodexProviderKeyEnvKey) isCodexProviderKey() {}
func (CodexProviderKeyInline) isCodexProviderKey() {}

func (k CodexProviderKeyEnvKey) String() string { return fmt.Sprintf("EnvKey(%q)", string(k)) }

// String: an inline key must never reach a log or an error, so it is
// redacted.
func (k CodexProviderKeyInline) String() string { return `Inline("<redacted>")` }

func (k CodexProviderKeyInline) GoString() string { return k.String() }

// String: a provider's inline key must never reach a log or an error, so it
// is redacted.
func (p CustomCodexProvider) String() string {
	envKey := "None"
	if p.EnvKey != nil {
		envKey = fmt.Sprintf("Some(%q)", *p.EnvKey)
	}
	token := "None"
	if p.BearerToken != nil {
		token = `Some("<redacted>")`
	}
	return fmt.Sprintf("CustomCodexProvider { id: %q, base_url: %q, env_key: %s, bearer_token: %s }", p.ID, p.BaseURL, envKey, token)
}

func (p CustomCodexProvider) GoString() string { return p.String() }

// Key is which key source this provider names (nil for none). When a table
// names both, `env_key` wins, matching the documented preference for the
// form that keeps the key out of the staged configuration.
func (p *CustomCodexProvider) Key() CodexProviderKey {
	switch {
	case p.EnvKey != nil:
		return CodexProviderKeyEnvKey(*p.EnvKey)
	case p.BearerToken != nil:
		return CodexProviderKeyInline(*p.BearerToken)
	}
	return nil
}

func (p *CodexProvider) ID() string {
	switch definition := p.Definition.(type) {
	case CodexProviderDefinitionBuiltIn:
		return definition.Provider.ID()
	case CodexProviderDefinitionCustom:
		return definition.Provider.ID
	}
	panic("codex provider without a definition")
}

func (p *CodexProvider) BuiltIn() (BuiltInCodexProvider, bool) {
	if definition, ok := p.Definition.(CodexProviderDefinitionBuiltIn); ok {
		return definition.Provider, true
	}
	return "", false
}

func (p *CodexProvider) Custom() *CustomCodexProvider {
	if definition, ok := p.Definition.(CodexProviderDefinitionCustom); ok {
		return &definition.Provider
	}
	return nil
}

func (p *CodexProvider) UsesAWSCredentials() bool {
	provider, ok := p.BuiltIn()
	return ok && provider.UsesAWSCredentials()
}

// UsesCodexLogin reports whether this provider uses Codex's native OpenAI
// login file. The built-in `openai` provider is equivalent to leaving
// `model_provider` unset; every other explicit provider has its own
// authentication path.
func (p *CodexProvider) UsesCodexLogin() bool {
	provider, ok := p.BuiltIn()
	return ok && provider.UsesCodexLogin()
}

// NeedsNoAuthentication reports whether this provider does not require
// credentials at all.
func (p *CodexProvider) NeedsNoAuthentication() bool {
	provider, ok := p.BuiltIn()
	return ok && provider.NeedsNoAuthentication()
}

// SkipsLoginFileSync reports whether the provider has no harness login file
// to synchronize. Only the built-in `openai` provider uses Codex's own login;
// every other provider (Bedrock, the local providers, and any custom
// provider, whose key is named by `env_key` or written inline as
// `experimental_bearer_token`) authenticates another way, so it has no
// `auth.json` to converge.
func (p *CodexProvider) SkipsLoginFileSync() bool { return !p.UsesCodexLogin() }

// Host is the host component of a custom provider's `base_url`, lowercased,
// when the URL parses. Built-in providers do not expose a custom base URL
// here.
func (p *CodexProvider) Host() (string, bool) {
	custom := p.Custom()
	if custom == nil {
		return "", false
	}
	parsed, err := url.Parse(custom.BaseURL)
	if err != nil || parsed.Host == "" {
		return "", false
	}
	return modelCatalogASCIILower(parsed.Hostname()), true
}

// Kind is which custom service this provider points at. Built-ins and
// unrecognized hosts are Other for Mjolnir's provider-specific quota and
// utility work.
func (p *CodexProvider) Kind() CodexProviderKind {
	host, ok := p.Host()
	if !ok {
		return CodexProviderKindOther
	}
	return CodexProviderKindFromHost(host)
}

// CodexProviderKind is which service a custom provider points at, as far as
// Mjolnir needs to treat it differently: Z.ai publishes a Coding Plan quota
// endpoint, DeepSeek serves usage-priced chat completions under the same
// `/v1` base URL, and everything else is an unknown usage-priced service.
type CodexProviderKind int

const (
	CodexProviderKindZai CodexProviderKind = iota
	CodexProviderKindDeepSeek
	CodexProviderKindOther
)

// CodexProviderKindFromHost classifies a host name. An unrecognized host is
// Other.
func CodexProviderKindFromHost(host string) CodexProviderKind {
	switch modelCatalogASCIILower(host) {
	case "api.z.ai", "open.bigmodel.cn":
		return CodexProviderKindZai
	case "api.deepseek.com":
		return CodexProviderKindDeepSeek
	}
	return CodexProviderKindOther
}

type CodexConfigFile struct {
	ModelProvider *string `toml:"model_provider"`
	// ModelCatalogJSON is the top-level key naming the JSON file Codex
	// advertises models from.
	ModelCatalogJSON *string                  `toml:"model_catalog_json"`
	ModelProviders   map[string]ProviderTable `toml:"model_providers"`
}

type ProviderTable struct {
	BaseURL                 *string `toml:"base_url"`
	WireAPI                 *string `toml:"wire_api"`
	EnvKey                  *string `toml:"env_key"`
	ExperimentalBearerToken *string `toml:"experimental_bearer_token"`
}

// OverlaySuffix is what follows an overlay's name in its file name: `ds` is
// `ds.config.toml`.
const OverlaySuffix = ".config.toml"

// IsOverlayName reports whether name is a Codex profile overlay name as
// `codex --profile` takes it: a plain name of ASCII letters, digits, `-` and
// `_`. Codex refuses anything else, including dots and path separators.
func IsOverlayName(name string) bool {
	if name == "" {
		return false
	}
	for i := 0; i < len(name); i++ {
		c := name[i]
		if !(harnessRuntimeIsASCIIAlphanumeric(c) || c == '-' || c == '_') {
			return false
		}
	}
	return true
}

// OverlayNames is the overlay files in home, by name, sorted. An unreadable
// home has none.
func OverlayNames(home string) []string {
	entries, err := os.ReadDir(home)
	if err != nil {
		return []string{}
	}
	names := []string{}
	for _, entry := range entries {
		name, ok := strings.CutSuffix(entry.Name(), OverlaySuffix)
		if !ok || !IsOverlayName(name) {
			continue
		}
		if info, err := os.Stat(filepath.Join(home, entry.Name())); err == nil && info.Mode().IsRegular() {
			names = append(names, name)
		}
	}
	sort.Strings(names)
	return names
}

// DescribeConfig is how the configuration of home with overlay is named in
// messages.
func DescribeConfig(home string, overlay *string) string {
	base := filepath.Join(home, "config.toml")
	if overlay == nil {
		return base
	}
	return fmt.Sprintf("%s layered with %s%s", base, *overlay, OverlaySuffix)
}

// ConfigFiles is the raw files a Codex configuration is layered from, as
// read: nil for a file that does not exist. Read separately from Layer so a
// poller can tell whether anything changed before it layers again.
type ConfigFiles struct {
	Base *string
	// Overlay is the overlay's name and its text.
	Overlay *ConfigFilesOverlay
}

// ConfigFilesOverlay is the overlay half of ConfigFiles: Rust's
// `(String, Option<String>)`.
type ConfigFilesOverlay struct {
	Name string
	Text *string
}

// ConfigFilesRead reads `<home>/config.toml` and, when named,
// `<home>/<overlay>.config.toml`. Only a file that exists but cannot be read
// is an error here.
func ConfigFilesRead(home string, overlay *string) (ConfigFiles, error) {
	read := func(path string) (*string, error) {
		data, err := os.ReadFile(path)
		if errors.Is(err, fs.ErrNotExist) {
			return nil, nil
		}
		if err != nil {
			return nil, fmt.Errorf("read %s: %w", path, err)
		}
		text := string(data)
		return &text, nil
	}
	base, err := read(filepath.Join(home, "config.toml"))
	if err != nil {
		return ConfigFiles{}, err
	}
	files := ConfigFiles{Base: base}
	if overlay != nil {
		name := *overlay
		files.Overlay = &ConfigFilesOverlay{Name: name}
		// An invalid name is refused by Layer; never join it to a path.
		if IsOverlayName(name) {
			text, err := read(filepath.Join(home, name+OverlaySuffix))
			if err != nil {
				return ConfigFiles{}, err
			}
			files.Overlay.Text = text
		}
	}
	return files, nil
}

// Layer is the configuration Codex runs with: `config.toml`, with the overlay
// merged over it the way `codex --profile` merges it. Tables merge key by key
// at every depth; any other value in the overlay replaces the base value.
// home only names the files in messages.
//
// Nil when there is no `config.toml` and no overlay. A missing `config.toml`
// is a home whose harness has not been set up yet, and discovery creates such
// profiles before their homes exist. A named overlay that does not exist is
// an error: Codex itself would silently run without it, and the session would
// not be the one that was asked for.
func (f *ConfigFiles) Layer(home string) (*string, error) {
	if f.Overlay == nil {
		if f.Base == nil {
			return nil, nil
		}
		base := *f.Base
		return &base, nil
	}
	name := f.Overlay.Name
	if !IsOverlayName(name) {
		return nil, fmt.Errorf("invalid Codex profile name %s; use ASCII letters, digits, '-' or '_'", debugString(name))
	}
	path := filepath.Join(home, "config.toml")
	overlayPath := filepath.Join(home, name+OverlaySuffix)
	if f.Overlay.Text == nil {
		return nil, fmt.Errorf("Codex profile %s has no file %s", debugString(name), overlayPath)
	}
	merged := map[string]any{}
	if f.Base != nil {
		if _, err := toml.Decode(*f.Base, &merged); err != nil {
			return nil, fmt.Errorf("parse Codex configuration %s: %w", path, err)
		}
	}
	layer := map[string]any{}
	if _, err := toml.Decode(*f.Overlay.Text, &layer); err != nil {
		return nil, fmt.Errorf("parse Codex configuration %s: %w", overlayPath, err)
	}
	mergeTables(merged, layer)
	var rendered strings.Builder
	encoder := toml.NewEncoder(&rendered)
	encoder.Indent = ""
	if err := encoder.Encode(merged); err != nil {
		return nil, fmt.Errorf("render %s: %w", DescribeConfig(home, &name), err)
	}
	text := rendered.String()
	return &text, nil
}

// EffectiveConfig is ConfigFilesRead and then Layer: the configuration Codex
// runs with from home and overlay.
func EffectiveConfig(home string, overlay *string) (*string, error) {
	files, err := ConfigFilesRead(home, overlay)
	if err != nil {
		return nil, err
	}
	return files.Layer(home)
}

func mergeTables(base map[string]any, layer map[string]any) {
	for key, value := range layer {
		baseTable, baseIsTable := base[key].(map[string]any)
		layerTable, layerIsTable := value.(map[string]any)
		if baseIsTable && layerIsTable {
			mergeTables(baseTable, layerTable)
			continue
		}
		base[key] = value
	}
}

// ReadCodexProvider is Rust's `codex_provider` function (renamed: the type
// has the same Go name). It reads the provider from EffectiveConfig; nil when
// there is no configuration or it names no `model_provider`.
func ReadCodexProvider(home string, overlay *string) (*CodexProvider, error) {
	text, err := EffectiveConfig(home, overlay)
	if err != nil || text == nil {
		return nil, err
	}
	return ParseConfig(*text, DescribeConfig(home, overlay))
}

// ParseConfig parses the text of a Codex `config.toml`. path names it in
// error messages.
func ParseConfig(text, path string) (*CodexProvider, error) {
	var file CodexConfigFile
	if err := decodeCodexConfigFile(text, &file); err != nil {
		return nil, fmt.Errorf("parse Codex configuration %s: %w", path, err)
	}
	if file.ModelProvider == nil {
		return nil, nil
	}
	id := *file.ModelProvider
	modelCatalogJSON := file.ModelCatalogJSON
	if provider, ok := BuiltInCodexProviderFromID(id); ok {
		return &CodexProvider{
			Definition:       CodexProviderDefinitionBuiltIn{Provider: provider},
			ModelCatalogJSON: modelCatalogJSON,
		}, nil
	}
	table, ok := file.ModelProviders[id]
	if !ok {
		return nil, fmt.Errorf("%s names model_provider %s, which is not a Codex built-in provider, and has no [model_providers.%s] custom-provider table", path, debugString(id), id)
	}
	if table.BaseURL == nil {
		return nil, fmt.Errorf("%s is missing base_url for model provider %s", path, debugString(id))
	}
	switch {
	case table.WireAPI == nil:
		return nil, fmt.Errorf("%s is missing wire_api = \"responses\" for model provider %s", path, debugString(id))
	case *table.WireAPI != "responses":
		return nil, fmt.Errorf("%s sets wire_api = %s for model provider %s; Codex only supports \"responses\"", path, debugString(*table.WireAPI), debugString(id))
	}
	trimmed := func(value *string) *string {
		if value == nil {
			return nil
		}
		text := strings.TrimSpace(*value)
		if text == "" {
			return nil
		}
		return &text
	}
	bearerToken := trimmed(table.ExperimentalBearerToken)
	envKey := trimmed(table.EnvKey)
	if envKey == nil && bearerToken == nil {
		return nil, fmt.Errorf("%s declares model provider %s with neither env_key nor experimental_bearer_token", path, debugString(id))
	}
	return &CodexProvider{
		Definition: CodexProviderDefinitionCustom{Provider: CustomCodexProvider{
			ID:          id,
			BaseURL:     *table.BaseURL,
			EnvKey:      envKey,
			BearerToken: bearerToken,
		}},
		ModelCatalogJSON: modelCatalogJSON,
	}, nil
}

// decodeCodexConfigFile reads the fields serde reads from a Codex
// `config.toml`: keys match exactly, unknown keys are ignored, and a present
// key of the wrong type is an error.
func decodeCodexConfigFile(text string, file *CodexConfigFile) error {
	document := map[string]any{}
	if _, err := toml.Decode(text, &document); err != nil {
		return err
	}
	optionalString := func(table map[string]any, key string) (*string, error) {
		value, ok := table[key]
		if !ok {
			return nil, nil
		}
		text, ok := value.(string)
		if !ok {
			return nil, fmt.Errorf("invalid type for `%s`: expected a string", key)
		}
		return &text, nil
	}
	var err error
	if file.ModelProvider, err = optionalString(document, "model_provider"); err != nil {
		return err
	}
	if file.ModelCatalogJSON, err = optionalString(document, "model_catalog_json"); err != nil {
		return err
	}
	file.ModelProviders = map[string]ProviderTable{}
	providers, ok := document["model_providers"]
	if !ok {
		return nil
	}
	tables, ok := providers.(map[string]any)
	if !ok {
		return errors.New("invalid type for `model_providers`: expected a table")
	}
	for id, value := range tables {
		table, ok := value.(map[string]any)
		if !ok {
			return fmt.Errorf("invalid type for `model_providers.%s`: expected a table", id)
		}
		var provider ProviderTable
		if provider.BaseURL, err = optionalString(table, "base_url"); err != nil {
			return err
		}
		if provider.WireAPI, err = optionalString(table, "wire_api"); err != nil {
			return err
		}
		if provider.EnvKey, err = optionalString(table, "env_key"); err != nil {
			return err
		}
		if provider.ExperimentalBearerToken, err = optionalString(table, "experimental_bearer_token"); err != nil {
			return err
		}
		file.ModelProviders[id] = provider
	}
	return nil
}
