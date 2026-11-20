// Port of mj-core/src/config/secrets.rs.
//
// Environment values that name a secret instead of holding it.
//
// A profile's or container's `environment` table accepts three forms per
// entry. A plain string is passed through as written. `{ from_env = "NAME" }`
// reads NAME from the process that loads the configuration.
// `{ from_secret = "NAME" }` reads NAME from `secrets.toml` beside
// `config.toml`, a flat table of string values that is never copied with the
// configuration.
//
// References are resolved while the configuration is read, so every consumer
// sees plain strings, and they are serialized as written, so a save keeps the
// reference rather than the value it stood for. A reference that cannot be
// resolved leaves its entry out of the values and is recorded instead: the
// profile or container that needs it is unusable until it resolves, but the
// configuration still loads. Environment.EnsureResolved reports it wherever
// that owner is about to be used.
//
// Rust scopes the active resolver and the sources-only switch to the reading
// thread. Here they belong to the reading goroutine: a scope records them
// under the goroutine's id for the duration of its read, so a decode sees only
// its own scope, scopes nest as Rust's do, and concurrent reads on other
// goroutines are unaffected.

package core

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"iter"
	"os"
	"path/filepath"
	"reflect"
	"runtime"
	"sort"
	"strconv"
	"strings"
	"sync"
)

// SecretsFile is the secrets file's name, beside config.toml.
const SecretsFile = "secrets.toml"

// SecretsPath is this instance's secrets file.
func SecretsPath() string { return filepath.Join(ConfigDir(), SecretsFile) }

// SecretsPathBeside is the secrets file beside a configuration file.
func SecretsPathBeside(configPath string) string {
	return filepath.Join(filepath.Dir(configPath), SecretsFile)
}

// EnvironmentValue is one environment entry as the configuration spells it.
type EnvironmentValue interface {
	isEnvironmentValue()
	IsReference() bool
	name() string
	MarshalJSON() ([]byte, error)
}

type EnvironmentValueLiteral struct{ Value string }

// EnvironmentValueFromEnv is read from the loading process's environment.
type EnvironmentValueFromEnv struct{ Value string }

// EnvironmentValueFromSecret is read from the secrets file.
type EnvironmentValueFromSecret struct{ Value string }

func (EnvironmentValueLiteral) isEnvironmentValue()    {}
func (EnvironmentValueFromEnv) isEnvironmentValue()    {}
func (EnvironmentValueFromSecret) isEnvironmentValue() {}

func (EnvironmentValueLiteral) IsReference() bool    { return false }
func (EnvironmentValueFromEnv) IsReference() bool    { return true }
func (EnvironmentValueFromSecret) IsReference() bool { return true }

func (v EnvironmentValueLiteral) name() string    { return v.Value }
func (v EnvironmentValueFromEnv) name() string    { return v.Value }
func (v EnvironmentValueFromSecret) name() string { return v.Value }

func (v EnvironmentValueLiteral) MarshalJSON() ([]byte, error) { return json.Marshal(v.Value) }

func (v EnvironmentValueFromEnv) MarshalJSON() ([]byte, error) {
	var object serdeObject
	object.field("from_env", v.Value)
	return object.finish()
}

func (v EnvironmentValueFromSecret) MarshalJSON() ([]byte, error) {
	var object serdeObject
	object.field("from_secret", v.Value)
	return object.finish()
}

// UnmarshalEnvironmentValueJSON reads an EnvironmentValue: a string,
// `{ from_env = "NAME" }`, or `{ from_secret = "NAME" }`.
func UnmarshalEnvironmentValueJSON(data []byte) (EnvironmentValue, error) {
	var literal string
	if json.Unmarshal(data, &literal) == nil {
		return EnvironmentValueLiteral{Value: literal}, nil
	}
	decoder := json.NewDecoder(strings.NewReader(string(data)))
	opening, err := decoder.Token()
	if delimiter, ok := opening.(json.Delim); err != nil || !ok || delimiter != '{' {
		return nil, fmt.Errorf("invalid type: %s, expected a string, `{ from_env = \"NAME\" }`, or `{ from_secret = \"NAME\" }`",
			serdeUnexpected(data))
	}
	var found EnvironmentValue
	for decoder.More() {
		token, err := decoder.Token()
		if err != nil {
			return nil, err
		}
		key, _ := token.(string)
		var name string
		if err := decoder.Decode(&name); err != nil {
			return nil, err
		}
		var value EnvironmentValue
		switch key {
		case "from_env":
			value = EnvironmentValueFromEnv{Value: name}
		case "from_secret":
			value = EnvironmentValueFromSecret{Value: name}
		default:
			return nil, fmt.Errorf("unknown field `%s`, expected `from_env` or `from_secret`", key)
		}
		if found != nil {
			return nil, errors.New("an environment reference names one of from_env or from_secret, not both")
		}
		if strings.TrimSpace(value.name()) == "" {
			return nil, fmt.Errorf("%s names nothing", key)
		}
		found = value
	}
	if found == nil {
		return nil, errors.New("an environment reference needs from_env or from_secret")
	}
	return found, nil
}

// EnvironmentValueTypedReference interprets a TOML inline table or the JSON
// form Settings displays (Rust's EnvironmentValue::typed_reference). Anything
// else is a literal, which the caller validates in context: ok is false. With
// ok, err says the table is not a reference.
func EnvironmentValueTypedReference(text string) (value EnvironmentValue, ok bool, err error) {
	text = strings.TrimSpace(text)
	if !strings.HasPrefix(text, "{") {
		return nil, false, nil
	}
	parsed, decodeErr := UnmarshalEnvironmentValueJSON([]byte(text))
	if decodeErr != nil {
		parsed = environmentValueFromTOMLTable("value = " + text)
	}
	if parsed == nil || !parsed.IsReference() {
		return nil, true, errors.New("Enter a reference like { from_secret = \"NAME\" } or { from_env = \"NAME\" }.")
	}
	return parsed, true, nil
}

// environmentValueFromTOMLTable is Rust's
// `toml::from_str::<BTreeMap<String, EnvironmentValue>>(document)` followed
// by taking `value`: nil unless the document parses and every entry is an
// environment value.
func environmentValueFromTOMLTable(document string) EnvironmentValue {
	table, err := decodeTOMLDocument(document)
	if err != nil {
		return nil
	}
	var found EnvironmentValue
	for key, entry := range table {
		encoded, err := json.Marshal(entry)
		if err != nil {
			return nil
		}
		value, err := UnmarshalEnvironmentValueJSON(encoded)
		if err != nil {
			return nil
		}
		if key == "value" {
			found = value
		}
	}
	return found
}

// Environment is an environment table: the entries as written, and the values
// they stand for. Reading gives the resolved values; serializing gives the
// entries as written.
type Environment struct {
	sources  map[string]EnvironmentValue
	resolved map[string]string
	// unresolved holds each entry whose reference did not resolve, with what
	// it needs. Such an entry has no resolved value.
	unresolved map[string]string
}

func NewEnvironment() Environment { return Environment{} }

// EnvironmentFromSources resolves every entry with the active SecretResolver,
// recording the ones that do not resolve rather than failing the read.
func EnvironmentFromSources(sources map[string]EnvironmentValue) Environment {
	resolved := map[string]string{}
	unresolved := map[string]string{}
	for _, key := range sortedKeys(sources) {
		value, err := resolveActive(key, sources[key])
		if err != nil {
			unresolved[key] = err.Error()
			continue
		}
		resolved[key] = value
	}
	return Environment{sources: sources, resolved: resolved, unresolved: unresolved}.normalized()
}

// EnvironmentFromMap is From<BTreeMap<String, String>>: every value a
// literal entry.
func EnvironmentFromMap(resolved map[string]string) Environment {
	sources := make(map[string]EnvironmentValue, len(resolved))
	values := make(map[string]string, len(resolved))
	for key, value := range resolved {
		sources[key] = EnvironmentValueLiteral{Value: value}
		values[key] = value
	}
	return Environment{sources: sources, resolved: values}.normalized()
}

// inherit supplies key from the loading process's environment, as
// `{ from_env = "key" }` would, when the table does not name it. The value is
// not an entry, so a save never writes it. An unset variable adds nothing: the
// caller that needs the key reports it.
func (e *Environment) inherit(key string) {
	if _, named := e.sources[key]; named || !resolvesValues() {
		return
	}
	if value, ok := environmentFromProcess(key); ok {
		if e.resolved == nil {
			e.resolved = map[string]string{}
		}
		e.resolved[key] = value
	}
}

// environmentFromProcess is the value of key in the loading process's
// environment, as `{ from_env = "key" }` would read it, whether or not the
// table names it.
func environmentFromProcess(key string) (string, bool) {
	value, err := resolveActive(key, EnvironmentValueFromEnv{Value: key})
	return value, err == nil
}

// supply gives key a value that outranks the table's own entry for it. The
// value is not an entry, so a save never writes it, and it settles an entry
// whose reference did not resolve.
func (e *Environment) supply(key, value string) {
	delete(e.unresolved, key)
	if e.resolved == nil {
		e.resolved = map[string]string{}
	}
	e.resolved[key] = value
}

// refuse makes key unusable for reason, whatever the table says.
func (e *Environment) refuse(key, reason string) {
	delete(e.resolved, key)
	if e.unresolved == nil {
		e.unresolved = map[string]string{}
	}
	e.unresolved[key] = reason
}

// EnsureResolved fails, naming what each one needs, when any entry's
// reference did not resolve. The owner of this table must not be used until
// it does.
func (e Environment) EnsureResolved() error {
	if len(e.unresolved) == 0 {
		return nil
	}
	keys := sortedKeys(e.unresolved)
	reasons := make([]string, len(keys))
	for i, key := range keys {
		reasons[i] = e.unresolved[key]
	}
	return errors.New(strings.Join(reasons, "; "))
}

func (e Environment) IsEmpty() bool { return len(e.sources) == 0 }

// Sources is the entries as the configuration spells them.
func (e Environment) Sources() map[string]EnvironmentValue { return e.sources }

// IsSecretSourced reports whether key's value came from somewhere other than
// the file itself: a from_env or from_secret reference, or a variable inherit
// supplied. Such a value must not appear on a command line.
func (e Environment) IsSecretSourced(key string) bool {
	if value, ok := e.sources[key]; ok {
		return value.IsReference()
	}
	_, ok := e.resolved[key]
	return ok
}

// InheritedNames is the names of the variables inherit supplied, which no
// entry spells, in name order.
func (e Environment) InheritedNames() iter.Seq[string] {
	return func(yield func(string) bool) {
		for _, key := range sortedKeys(e.resolved) {
			if _, spelled := e.sources[key]; spelled {
				continue
			}
			if !yield(key) {
				return
			}
		}
	}
}

// Resolved is the values the entries stand for. Rust's Deref target: read
// values through it (Get is a shortcut).
func (e Environment) Resolved() map[string]string { return e.resolved }

// Get is the resolved value of key.
func (e Environment) Get(key string) (string, bool) {
	value, ok := e.resolved[key]
	return value, ok
}

// ContainsKey reports whether key has a resolved value.
func (e Environment) ContainsKey(key string) bool {
	_, ok := e.resolved[key]
	return ok
}

// IntoResolved is the values the entries stand for, for a caller that
// consumes the table.
func (e Environment) IntoResolved() map[string]string { return e.resolved }

// Insert sets a literal entry, returning the value it replaced.
func (e *Environment) Insert(key, value string) (string, bool) {
	if e.sources == nil {
		e.sources = map[string]EnvironmentValue{}
	}
	if e.resolved == nil {
		e.resolved = map[string]string{}
	}
	e.sources[key] = EnvironmentValueLiteral{Value: value}
	delete(e.unresolved, key)
	previous, had := e.resolved[key]
	e.resolved[key] = value
	return previous, had
}

func (e *Environment) Remove(key string) (string, bool) {
	delete(e.sources, key)
	delete(e.unresolved, key)
	previous, had := e.resolved[key]
	delete(e.resolved, key)
	return previous, had
}

// normalized gives every empty table one representation, nil, so tables Rust
// compares equal also compare equal with reflect.DeepEqual.
func (e Environment) normalized() Environment {
	if len(e.sources) == 0 {
		e.sources = nil
	}
	if len(e.resolved) == 0 {
		e.resolved = nil
	}
	if len(e.unresolved) == 0 {
		e.unresolved = nil
	}
	return e
}

// Equal is Environment's PartialEq: the entries, the values and what each
// unresolved entry needs.
func (e Environment) Equal(other Environment) bool {
	return reflect.DeepEqual(e.normalized(), other.normalized())
}

func (e *Environment) Clear() {
	e.sources = nil
	e.resolved = nil
	e.unresolved = nil
}

// MarshalJSON writes the entries as written.
func (e Environment) MarshalJSON() ([]byte, error) {
	if e.sources == nil {
		return []byte("{}"), nil
	}
	return json.Marshal(e.sources)
}

// UnmarshalJSON reads the entries and resolves them with the active
// SecretResolver, unless a sources-only read is in progress.
func (e *Environment) UnmarshalJSON(data []byte) error {
	var raw map[string]json.RawMessage
	if err := json.Unmarshal(data, &raw); err != nil {
		return fmt.Errorf("invalid type: %s, expected a map", serdeUnexpected(data))
	}
	sources := make(map[string]EnvironmentValue, len(raw))
	for key, encoded := range raw {
		value, err := UnmarshalEnvironmentValueJSON(encoded)
		if err != nil {
			return err
		}
		sources[key] = value
	}
	if !resolvesValues() {
		*e = Environment{sources: sources}.normalized()
		return nil
	}
	*e = EnvironmentFromSources(sources)
	return nil
}

// SecretStore is where secret values are read from.
type SecretStore interface {
	isSecretStore()
}

type SecretStoreFile struct{ Value string }

type SecretStoreValues struct{ Value map[string]string }

func (SecretStoreFile) isSecretStore()   {}
func (SecretStoreValues) isSecretStore() {}

// SecretResolver resolves environment references for one configuration read.
type SecretResolver struct {
	// process is nil to read the real process environment.
	process map[string]string
	secrets SecretStore
}

// SecretResolverBeside reads secrets from the file beside configPath and the
// environment from the process.
func SecretResolverBeside(configPath string) SecretResolver {
	return SecretResolver{secrets: SecretStoreFile{Value: SecretsPathBeside(configPath)}}
}

// SecretResolverForInstance reads secrets from this instance's file and the
// environment from the process.
func SecretResolverForInstance() SecretResolver {
	return SecretResolver{secrets: SecretStoreFile{Value: SecretsPath()}}
}

// SecretResolverFixed uses fixed values for both sources.
func SecretResolverFixed(process, secrets map[string]string) SecretResolver {
	if process == nil {
		process = map[string]string{}
	}
	return SecretResolver{process: process, secrets: SecretStoreValues{Value: secrets}}
}

func (r SecretResolver) resolve(key string, value EnvironmentValue) (string, error) {
	switch value := value.(type) {
	case EnvironmentValueLiteral:
		return value.Value, nil
	case EnvironmentValueFromEnv:
		name := value.Value
		var found string
		var ok bool
		if r.process != nil {
			found, ok = r.process[name]
		} else {
			found, ok = os.LookupEnv(name)
		}
		if ok && strings.TrimSpace(found) != "" {
			return found, nil
		}
		return "", fmt.Errorf("%s = { from_env = %s } needs %s set, and not empty, in the environment of the process reading the configuration",
			key, debugString(name), name)
	case EnvironmentValueFromSecret:
		name := value.Value
		switch store := r.secrets.(type) {
		case SecretStoreValues:
			if found, ok := store.Value[name]; ok {
				return found, nil
			}
			return "", fmt.Errorf("%s = { from_secret = %s } names a secret that is not defined", key, debugString(name))
		case SecretStoreFile:
			secrets, err := LoadSecrets(store.Value)
			if err != nil {
				return "", fmt.Errorf("%s = { from_secret = %s } needs %s: %w", key, debugString(name), store.Value, err)
			}
			if found, ok := secrets[name]; ok && strings.TrimSpace(found) != "" {
				return found, nil
			}
			return "", fmt.Errorf("%s = { from_secret = %s } needs `%s = \"...\"` in %s", key, debugString(name), name, store.Value)
		}
		panic(fmt.Sprintf("unknown secret store %T", r.secrets))
	}
	panic(fmt.Sprintf("unknown environment value %T", value))
}

// secretScope is one goroutine's read scope: Rust's thread_local! ACTIVE and
// SOURCES_ONLY.
type secretScope struct {
	// resolver is the resolver of the scoped read in progress, if any.
	resolver *SecretResolver
	// sourcesOnly is set while a projection is read with
	// WithEnvironmentSourcesOnly.
	sourcesOnly bool
}

// secretScopes holds the open scope of each goroutine reading inside one,
// keyed by goroutine id. A goroutine with no entry has the zero scope.
var secretScopes sync.Map

// currentGoroutineID is the runtime's id for the calling goroutine, read from
// the first line of its stack trace ("goroutine 123 [running]:").
func currentGoroutineID() uint64 {
	var buffer [64]byte
	line := buffer[:runtime.Stack(buffer[:], false)]
	line = bytes.TrimPrefix(line, []byte("goroutine "))
	if end := bytes.IndexByte(line, ' '); end >= 0 {
		line = line[:end]
	}
	id, err := strconv.ParseUint(string(line), 10, 64)
	if err != nil {
		panic("read the goroutine id from its stack trace: " + err.Error())
	}
	return id
}

func currentSecretScope() secretScope {
	if scope, ok := secretScopes.Load(currentGoroutineID()); ok {
		return scope.(secretScope)
	}
	return secretScope{}
}

// withSecretScope runs read with change applied to this goroutine's scope,
// restoring the previous scope when read returns or panics.
func withSecretScope[T any](change func(*secretScope), read func() T) T {
	id := currentGoroutineID()
	previous, had := secretScopes.Load(id)
	scope := secretScope{}
	if had {
		scope = previous.(secretScope)
	}
	change(&scope)
	secretScopes.Store(id, scope)
	defer func() {
		if had {
			secretScopes.Store(id, previous)
		} else {
			secretScopes.Delete(id)
		}
	}()
	return read()
}

// WithEnvironmentSourcesOnly deserializes a configuration projection without
// reading credentials or secret files. Its environments retain their
// configured sources, but have no resolved values: serialize them for
// background resolution before use.
func WithEnvironmentSourcesOnly[T any](read func() T) T {
	return withSecretScope(func(scope *secretScope) { scope.sourcesOnly = true }, read)
}

// resolvesValues is false while a projection is read with
// WithEnvironmentSourcesOnly, which must neither resolve values nor read files
// to find them.
func resolvesValues() bool { return !currentSecretScope().sourcesOnly }

// WithSecretResolver runs read with resolver answering every environment
// reference it meets. Without one, references resolve against this
// instance's secrets file and the process environment.
func WithSecretResolver[T any](resolver SecretResolver, read func() T) T {
	return withSecretScope(func(scope *secretScope) { scope.resolver = &resolver }, read)
}

func resolveActive(key string, value EnvironmentValue) (string, error) {
	if resolver := currentSecretScope().resolver; resolver != nil {
		return resolver.resolve(key, value)
	}
	return SecretResolverForInstance().resolve(key, value)
}

// LoadSecrets reads a secrets file: a flat table of string values.
func LoadSecrets(path string) (map[string]string, error) {
	contents, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("read secrets file %s: %w", path, err)
	}
	table, err := decodeTOMLDocument(string(contents))
	if err != nil {
		return nil, fmt.Errorf("parse secrets file %s: %w", path, err)
	}
	secrets := make(map[string]string, len(table))
	for _, name := range sortedKeys(table) {
		value, ok := table[name].(string)
		if !ok {
			return nil, fmt.Errorf("%s: `%s` must be a string; the secrets file holds only `NAME = \"value\"` lines", path, name)
		}
		secrets[name] = value
	}
	return secrets, nil
}

// SecretsPermissionProblem says why a secrets file is not private to its
// owner, if it is not.
func SecretsPermissionProblem(path string) (string, bool, error) {
	if runtime.GOOS == "windows" {
		return "", false, nil
	}
	info, err := os.Stat(path)
	if err != nil {
		return "", false, fmt.Errorf("inspect %s: %w", path, err)
	}
	mode := info.Mode().Perm()
	if mode&0o077 != 0 {
		return fmt.Sprintf("%s is readable by other users (mode %03o)", path, uint32(mode)), true, nil
	}
	return "", false, nil
}

// LooksLikeCredential reports whether an environment variable's name suggests
// a credential, so a literal value under it belongs in the secrets file.
func LooksLikeCredential(name string) bool {
	upper := strings.Map(func(r rune) rune {
		if r >= 'a' && r <= 'z' {
			return r - ('a' - 'A')
		}
		return r
	}, name)
	for _, word := range []string{"API_KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL"} {
		if strings.Contains(upper, word) {
			return true
		}
	}
	return strings.HasSuffix(upper, "_KEY")
}

// sortedKeys is a map's keys in order, for the BTreeMap iteration order the
// Rust code relies on.
func sortedKeys[V any](values map[string]V) []string {
	keys := make([]string, 0, len(values))
	for key := range values {
		keys = append(keys, key)
	}
	sort.Strings(keys)
	return keys
}
