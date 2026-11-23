// Port of mj-worker/src/acp/guardian/reviewer_profile.rs.

// A reviewer uses the session's login and provider settings in a private
// home with no session-owned hooks, permission exceptions, or MCP
// registrations.

package worker

import (
	"bytes"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BurntSushi/toml"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// reviewerSessionEnvironment is the session-owned environment a reviewer
// never inherits.
var reviewerSessionEnvironment = []string{
	core.SessionManagedSubagentEnv,
	core.SessionMessageMCPEnv,
	core.SessionRawLaunchRecordEnv,
	"MJOLNIR_MAILBOX_HOOK",
	"MUSE_ACP_ALLOW_COMMAND_PREFIXES",
}

func reviewerRemoveSessionEnvironment(environment map[string]string) {
	for _, name := range reviewerSessionEnvironment {
		delete(environment, name)
	}
}

// reviewerProfileStage stages the reviewer's private home from the
// session's (Unix workers only): the login and provider settings, without
// the session's hooks, permission exceptions, or MCP registrations. It
// points environment and args at the new home and returns its directory,
// which the caller removes once the reviewer is gone; "" when the harness
// keeps no staged home to copy.
func reviewerProfileStage(harness core.HarnessKind, environment map[string]string, args []string) (string, error) {
	if harness == core.HarnessKindOpenCode || harness == core.HarnessKindDsh {
		return "", nil
	}
	value, ok := environment[harness.HomeEnv()]
	if !ok {
		// Standalone bridges can supply a self-contained command instead of
		// a worker-staged harness profile.
		return "", nil
	}
	source := harness.HomeFromEnvironment(value)
	profile, err := os.MkdirTemp("", "mj-guardian-")
	if err != nil {
		return "", err
	}
	staged := false
	defer func() {
		if !staged {
			_ = os.RemoveAll(profile)
		}
	}()
	home := profile
	if harness.NestedHome() {
		home = filepath.Join(profile, harness.ID())
	}
	if err := os.MkdirAll(home, 0o755); err != nil {
		return "", err
	}
	if harness == core.HarnessKindKimi {
		// Keep the OAuth files and their locks under one owner. Nothing else
		// from the session's Kimi home (notably mcp.json) is shared.
		credentials := filepath.Join(source, "credentials")
		if _, err := os.Lstat(credentials); err == nil {
			if err := os.Symlink(credentials, filepath.Join(home, "credentials")); err != nil {
				return "", err
			}
		} else if !errors.Is(err, fs.ErrNotExist) {
			return "", err
		}
		if err := reviewerCopyFile(source, home, "device_id"); err != nil {
			return "", err
		}
	} else if err := reviewerCopyFile(source, home, harness.CredentialFileName()); err != nil {
		return "", err
	}
	switch harness {
	case core.HarnessKindCodex:
		if err := reviewerStageTOML(source, home, func(config map[string]any) {
			for _, key := range []string{"hooks", "mcp_servers", "plugins", "sandbox_mode"} {
				delete(config, key)
			}
			config["default_permissions"] = ":read-only"
			if catalog, ok := config["model_catalog_json"].(string); ok && !filepath.IsAbs(catalog) {
				config["model_catalog_json"] = core.Text(filepath.Join(source, catalog))
			}
		}); err != nil {
			return "", err
		}
	case core.HarnessKindClaude:
		if err := reviewerStageJSON(source, home, ".claude.json", "mcpServers", "projects"); err != nil {
			return "", err
		}
		if err := reviewerStageJSON(source, home, "settings.json",
			"hooks", "permissions", "enabledPlugins", "extraKnownMarketplaces"); err != nil {
			return "", err
		}
	case core.HarnessKindAntigravity:
		homeArgs := harness.HomeCLIArgs(home)
		for index := 0; index+1 < len(args); index++ {
			if args[index] == homeArgs[0] {
				args[index+1] = homeArgs[1]
				break
			}
		}
	case core.HarnessKindKimi, core.HarnessKindGrok:
		if err := reviewerStageTOML(source, home, func(config map[string]any) {
			for _, key := range []string{"hooks", "mcp_servers", "plugins", "permission"} {
				delete(config, key)
			}
		}); err != nil {
			return "", err
		}
		if harness == core.HarnessKindGrok {
			if err := reviewerCopyFile(source, home, "agent_id"); err != nil {
				return "", err
			}
		}
	case core.HarnessKindMuse:
		if err := reviewerStageJSON(source, home, "settings.json", "hooks", "permissions"); err != nil {
			return "", err
		}
		if err := reviewerCopyFile(source, home, "trust.json"); err != nil {
			return "", err
		}
		catalog := filepath.Join(".data", "muse", "model-catalog")
		if info, err := os.Stat(filepath.Join(source, catalog)); err == nil && info.IsDir() {
			entries, err := os.ReadDir(filepath.Join(source, catalog))
			if err != nil {
				return "", err
			}
			for _, entry := range entries {
				if entry.Type().IsRegular() {
					if err := reviewerCopyFile(source, home, filepath.Join(catalog, entry.Name())); err != nil {
						return "", err
					}
				}
			}
		}
	default:
		panic("unreachable: OpenCode and DSH stage no reviewer home")
	}
	harness.ConfigureStagedHomeEnvironment(home, environment)
	staged = true
	return profile, nil
}

// reviewerReadOptional reads path; ok is false when it does not exist.
func reviewerReadOptional(path string) ([]byte, bool, error) {
	data, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		return nil, false, nil
	}
	if err != nil {
		return nil, false, fmt.Errorf("read reviewer input %s: %w", path, err)
	}
	return data, true, nil
}

func reviewerCopyFile(source, home, file string) error {
	data, ok, err := reviewerReadOptional(filepath.Join(source, file))
	if err != nil || !ok {
		return err
	}
	return core.AtomicWrite(filepath.Join(home, file), data)
}

func reviewerStageTOML(source, home string, edit func(config map[string]any)) error {
	data, ok, err := reviewerReadOptional(filepath.Join(source, "config.toml"))
	if err != nil || !ok {
		return err
	}
	config := map[string]any{}
	if err := toml.Unmarshal(data, &config); err != nil {
		return fmt.Errorf("parse staged reviewer TOML configuration: %w", err)
	}
	edit(config)
	var encoded bytes.Buffer
	if err := toml.NewEncoder(&encoded).Encode(config); err != nil {
		return err
	}
	return core.AtomicWrite(filepath.Join(home, "config.toml"), encoded.Bytes())
}

func reviewerStageJSON(source, home, file string, remove ...string) error {
	data, ok, err := reviewerReadOptional(filepath.Join(source, file))
	if err != nil || !ok {
		return err
	}
	value, err := jsonvalue.Parse(data)
	config, isObject := value.(jsonvalue.Object)
	if err != nil || !isObject {
		if err == nil {
			err = errors.New("invalid type: expected a map")
		}
		return fmt.Errorf("parse staged reviewer %s: %w", file, err)
	}
	for _, key := range remove {
		// serde_json's Map::remove with preserve_order.
		config.SwapDelete(key)
	}
	encoded, err := jsonvalue.Marshal(config)
	if err != nil {
		return err
	}
	return core.AtomicWrite(filepath.Join(home, file), encoded)
}
