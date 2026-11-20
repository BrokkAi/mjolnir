// Port of mj-core/src/codex_catalog.rs.

// The Codex model catalog and the rule that picks a Guardian reviewer model.
//
// Codex advertises the models a session may pick from a JSON file named by
// the top-level `model_catalog_json` key in its `config.toml`. The file is an
// object with a `models` array; each entry is an object whose `slug` is the
// model name. Codex fetches this list from its own service only for ChatGPT
// logins, so a profile that authenticates with an API key against another
// provider would otherwise advertise OpenAI's built-in model names. Mjolnir
// therefore fetches the provider's catalog and stages it as `models.json`.
//
// "Guardian" is Codex's review mode: before an action leaves the sandbox a
// second model reviews it and returns an allow or deny outcome. Codex picks
// that reviewer from the session model's catalog entry field
// `auto_review_model_override`, falling back to reviewing with the session
// model itself. Mjolnir stamps the override so a heavyweight session model is
// not also its own reviewer.

package core

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"slices"
	"strconv"
	"strings"
)

// CodexCatalog is a Codex `models.json` document. Entries are kept as JSON
// objects (decoded with json.Decoder.UseNumber, so numbers keep their text)
// so unknown Codex fields pass through unchanged.
type CodexCatalog struct {
	Models []map[string]any
}

// Slugs is every entry's `slug`, in catalog order.
func (c *CodexCatalog) Slugs() []string {
	slugs := []string{}
	for _, model := range c.Models {
		if slug, ok := model["slug"].(string); ok {
			slugs = append(slugs, slug)
		}
	}
	return slugs
}

// ToJSON serializes back to the `{"models": [...]}` document Codex reads.
func (c *CodexCatalog) ToJSON() string {
	models := make([]any, len(c.Models))
	for i, model := range c.Models {
		models[i] = model
	}
	// serde_json's to_string_pretty: two-space indent, sorted object keys,
	// no HTML escapes, no trailing newline.
	var body bytes.Buffer
	encoder := json.NewEncoder(&body)
	encoder.SetEscapeHTML(false)
	encoder.SetIndent("", "  ")
	if err := encoder.Encode(map[string]any{"models": models}); err != nil {
		panic(fmt.Sprintf("catalog objects are serializable: %v", err))
	}
	return strings.TrimSuffix(body.String(), "\n")
}

// codexCatalogDecode parses one JSON document, keeping numbers as their
// text and refusing trailing data as serde_json does.
func codexCatalogDecode(data []byte) (any, error) {
	decoder := json.NewDecoder(bytes.NewReader(data))
	decoder.UseNumber()
	var value any
	if err := decoder.Decode(&value); err != nil {
		return nil, fmt.Errorf("parse model catalog as JSON: %w", err)
	}
	if _, err := decoder.Token(); err != io.EOF {
		return nil, errors.New("parse model catalog as JSON: trailing characters")
	}
	return value, nil
}

// codexCatalogArray is `value.get(key).and_then(Value::as_array)`.
func codexCatalogArray(value any, key string) ([]any, bool) {
	object, ok := value.(map[string]any)
	if !ok {
		return nil, false
	}
	array, ok := object[key].([]any)
	return array, ok
}

// CodexCatalogParse is Rust's `codex_catalog::parse` (prefixed: the name is
// activity/question_locator's): parse a provider's `GET /models` response
// body.
//
// Two shapes are accepted. The Codex shape, `{"models": [...]}`, is what Z.ai
// serves and is kept entry for entry. OpenAI's plain model list,
// `{"object": "list", "data": [{"id": "..."}]}`, is what DeepSeek and most
// OpenAI-compatible providers serve; it carries only ids, so each entry is
// translated into a full Codex entry using the conservative defaults in
// entryFromOpenAIModel. A body with neither array is an error naming both
// shapes.
func CodexCatalogParse(data []byte) (CodexCatalog, error) {
	value, err := codexCatalogDecode(data)
	if err != nil {
		return CodexCatalog{}, err
	}
	var entries []map[string]any
	if models, ok := codexCatalogArray(value, "models"); ok {
		entries = make([]map[string]any, 0, len(models))
		for _, model := range models {
			object, ok := model.(map[string]any)
			if !ok {
				return CodexCatalog{}, errors.New("model catalog entry is not a JSON object")
			}
			entries = append(entries, object)
		}
	} else if list, ok := codexCatalogArray(value, "data"); ok {
		entries = make([]map[string]any, 0, len(list))
		for position, model := range list {
			object, ok := model.(map[string]any)
			if !ok {
				return CodexCatalog{}, errors.New("model list entry is not a JSON object")
			}
			id, ok := object["id"].(string)
			if !ok {
				return CodexCatalog{}, errors.New("model list entry has no `id`")
			}
			var ownedBy *string
			if owner, ok := object["owned_by"].(string); ok {
				ownedBy = &owner
			}
			entries = append(entries, entryFromOpenAIModel(id, ownedBy, position))
		}
	} else {
		return CodexCatalog{}, errors.New("model catalog has neither a `models` array (Codex catalog format) nor a `data` array (OpenAI model list format)")
	}
	if len(entries) == 0 {
		return CodexCatalog{}, errors.New("model catalog lists no models")
	}
	for _, entry := range entries {
		backfillReasoningLevels(entry)
	}
	return CodexCatalog{Models: entries}, nil
}

// backfillReasoningLevels fills in reasoning-effort levels for a known model
// family when the provider's catalog does not state them.
//
// A provider that serves OpenAI's plain model list (DeepSeek) gives no
// capabilities at all, and some Codex-shape catalogs list a model with an
// empty `supported_reasoning_levels` (Z.ai returns `glm-5-turbo` that way).
// Without levels, Codex advertises no effort selector for that model, so
// Mjolnir cannot apply the profile's effort and the session fails with "ACP
// bridge does not expose a effort selector". This backfill uses
// knownReasoningLevels to supply the levels the provider omitted. It only
// fills an absent or empty list, so a provider that does state its levels
// keeps them, and a profile's own `models.json` override still wins because
// MergeOverrides runs later.
func backfillReasoningLevels(entry map[string]any) {
	slug, ok := entry["slug"].(string)
	if !ok {
		return
	}
	if listed, ok := entry["supported_reasoning_levels"].([]any); ok && len(listed) > 0 {
		return
	}
	levels, defaultLevel, ok := knownReasoningLevels(slug)
	if !ok {
		return
	}
	values := make([]any, 0, len(levels))
	for _, level := range levels {
		values = append(values, map[string]any{"effort": level[0], "description": level[1]})
	}
	entry["supported_reasoning_levels"] = values
	// Keep a default the provider already stated (Z.ai's `glm-5-turbo` says
	// `max` even while listing no levels); only supply one when it is missing.
	if _, present := entry["default_reasoning_level"]; !present {
		entry["default_reasoning_level"] = defaultLevel
	}
}

// knownReasoningLevels is reasoning-effort levels (effort, description) and
// the default for known model families, used only to fill a gap a provider's
// catalog left (see backfillReasoningLevels).
//
// The values come from the providers' own documentation (checked 2026-09):
// DeepSeek V4 Pro and V4 Flash both expose `low`, `high`, and `max`, and
// Z.ai's GLM 5.3 family exposes `low`, `high`, and `max` with `max` as its
// default. Matching is by model-id prefix so new point releases in a family
// are covered. A profile's `models.json` override is the way to correct any
// entry this table gets wrong.
func knownReasoningLevels(slug string) ([][2]string, string, bool) {
	deepseek := [][2]string{
		{"low", "Light reasoning"},
		{"high", "Deep reasoning"},
		{"max", "Maximum reasoning"},
	}
	glm := [][2]string{
		{"low", "Light reasoning"},
		{"high", "Enhanced reasoning"},
		{"max", "Deep reasoning"},
	}
	switch {
	case strings.HasPrefix(slug, "deepseek"):
		return deepseek, "high", true
	case strings.HasPrefix(slug, "glm"):
		return glm, "max", true
	}
	return nil, "", false
}

// knownContextLimits is the context window and automatic-compaction
// threshold, in tokens, for known model families whose providers list only
// ids.
//
// DeepSeek models (`deepseek-*`) use a 1,000,000-token window. DeepSeek's own
// harness (`deepseek-harness`, `compaction-basic`) compacts at 80% of the
// window, so Codex sessions on these models compact at the same 800,000
// tokens rather than at Codex's default of 90%. Codex clamps the threshold to
// 90% of the window, so a `models.json` override that shrinks the window
// still works.
func knownContextLimits(slug string) (window uint64, compactAt uint64, ok bool) {
	if strings.HasPrefix(slug, "deepseek-") {
		return 1_000_000, 800_000, true
	}
	return 0, 0, false
}

// ParseCodexShape parses a Codex-shape catalog only, for the user's optional
// `models.json` override file. The override file refines fetched entries, so
// it must speak the same language as the catalog it refines.
func ParseCodexShape(data []byte) (CodexCatalog, error) {
	value, err := codexCatalogDecode(data)
	if err != nil {
		return CodexCatalog{}, err
	}
	models, ok := codexCatalogArray(value, "models")
	if !ok {
		return CodexCatalog{}, errors.New("model catalog has no `models` array")
	}
	entries := make([]map[string]any, 0, len(models))
	for _, model := range models {
		object, ok := model.(map[string]any)
		if !ok {
			return CodexCatalog{}, errors.New("model catalog entry is not a JSON object")
		}
		entries = append(entries, object)
	}
	if len(entries) == 0 {
		return CodexCatalog{}, errors.New("model catalog lists no models")
	}
	return CodexCatalog{Models: entries}, nil
}

// entryFromOpenAIModel builds a Codex catalog entry from one id in an
// OpenAI-format model list (ownedBy is nil when absent).
//
// The list carries no capabilities, so the defaults are deliberately
// conservative: no reasoning levels here, a 128k context window unless
// knownContextLimits knows the family, and the plain shell tool.
// CodexCatalogParse then runs backfillReasoningLevels, which supplies levels
// for a known model family (DeepSeek and GLM), and a session on an unknown
// model still shows no effort choice rather than one the provider rejects. A
// user refines any entry with a `models.json` override file in the profile
// home.
func entryFromOpenAIModel(id string, ownedBy *string, position int) map[string]any {
	description := id
	if ownedBy != nil && *ownedBy != "" {
		description = fmt.Sprintf("%s (%s)", id, *ownedBy)
	}
	number := func(value uint64) json.Number { return json.Number(strconv.FormatUint(value, 10)) }
	entry := map[string]any{
		"slug":                         id,
		"display_name":                 id,
		"description":                  description,
		"supported_reasoning_levels":   []any{},
		"shell_type":                   "shell_command",
		"visibility":                   "list",
		"supported_in_api":             true,
		"priority":                     number(uint64(position)),
		"base_instructions":            "",
		"supports_reasoning_summaries": false,
		"default_reasoning_summary":    "none",
		"support_verbosity":            false,
		"apply_patch_tool_type":        "freeform",
		"truncation_policy":            map[string]any{"mode": "bytes", "limit": number(10000)},
	}
	contextWindow := uint64(128000)
	if window, limit, ok := knownContextLimits(id); ok {
		contextWindow = window
		entry["auto_compact_token_limit"] = number(limit)
	}
	entry["context_window"] = number(contextWindow)
	entry["max_context_window"] = number(contextWindow)
	entry["effective_context_window_percent"] = number(95)
	entry["supports_parallel_tool_calls"] = true
	entry["experimental_supported_tools"] = []any{}
	entry["input_modalities"] = []any{"text"}
	return entry
}

// MergeOverrides copies the user's override entries over the fetched
// catalog.
//
// An override entry whose `slug` the provider listed replaces those fields on
// the fetched entry and leaves the rest; a slug the provider did not list is
// appended, so a user can advertise a model the provider's list omits. This
// is how a user adds reasoning levels or a larger context window to a
// provider whose model list carries only ids.
func MergeOverrides(catalog *CodexCatalog, overrides *CodexCatalog) {
	for _, entry := range overrides.Models {
		slug, ok := entry["slug"].(string)
		if !ok {
			continue
		}
		var existing map[string]any
		for _, model := range catalog.Models {
			if found, ok := model["slug"].(string); ok && found == slug {
				existing = model
				break
			}
		}
		if existing != nil {
			for field, value := range entry {
				existing[field] = value
			}
			continue
		}
		// A slug the provider did not list becomes a complete entry:
		// start from the same conservative defaults a translated entry
		// gets, then overlay the user's fields. Pushing the override
		// verbatim would leave a partial entry that Codex rejects,
		// failing the launch, even though the docs promise a few fields
		// are enough to add a model.
		complete := entryFromOpenAIModel(slug, nil, len(catalog.Models))
		for field, value := range entry {
			complete[field] = value
		}
		catalog.Models = append(catalog.Models, complete)
	}
}

// GuardianReviewModel is the newest flash model among slugs, which Mjolnir
// uses as the Guardian reviewer.
//
// "Flash" is the vendor's name for its small, fast model; reviewing with it
// keeps Guardian checks cheap. Versions compare as the dotted number sequence
// that follows the first `-` in the slug, segment by segment and numerically,
// so `glm-5.10-flash` is newer than `glm-5.3-flash`. Slugs with no flash
// marker are ignored, and ok false means the catalog has no flash model and
// Codex should keep its own fallback of reviewing with the session model.
func GuardianReviewModel(slugs []string) (string, bool) {
	best, found := "", false
	for _, slug := range slugs {
		if !strings.Contains(modelCatalogASCIILower(slug), "flash") {
			continue
		}
		// Rust's max_by keeps the last of equal maxima.
		if !found || slices.Compare(versionOf(slug), versionOf(best)) >= 0 {
			best, found = slug, true
		}
	}
	return best, found
}

// versionOf is the dotted number sequence in a slug, as numbers.
// `glm-5.3-flash` is `[5, 3]`; a slug with no numbers is an empty vector,
// which sorts first.
func versionOf(slug string) []uint64 {
	parts := strings.FieldsFunc(slug, func(r rune) bool { return !(r >= '0' && r <= '9') && r != '.' })
	for _, part := range parts {
		if part[0] < '0' || part[0] > '9' {
			continue
		}
		version := []uint64{}
		for _, segment := range strings.Split(part, ".") {
			if value, err := strconv.ParseUint(segment, 10, 64); err == nil {
				version = append(version, value)
			}
		}
		return version
	}
	return []uint64{}
}

// StampReviewer sets `auto_review_model_override` on every catalog entry, so
// whichever model the session runs on reviews with reviewer.
func StampReviewer(catalog *CodexCatalog, reviewer string) {
	for _, model := range catalog.Models {
		model["auto_review_model_override"] = reviewer
	}
}

// IsLunaModel reports whether a Codex catalog id names a GPT Luna model: the
// bare alias `luna`, or a full id such as `gpt-5.10-luna` or `GPT-5.9-Luna`.
// Case-insensitive; matches when any `- _ . /`-separated part of the id
// equals `luna`.
func IsLunaModel(id string) bool {
	parts := strings.FieldsFunc(modelCatalogASCIILower(id), func(r rune) bool {
		return r == '-' || r == '_' || r == '.' || r == '/'
	})
	for _, part := range parts {
		if part == "luna" {
			return true
		}
	}
	return false
}
