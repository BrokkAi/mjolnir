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
	panic("unported: mj-core/src/codex_catalog.rs CodexCatalog::to_json")
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
	panic("unported: mj-core/src/codex_catalog.rs parse")
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
	panic("unported: mj-core/src/codex_catalog.rs backfill_reasoning_levels")
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
	panic("unported: mj-core/src/codex_catalog.rs parse_codex_shape")
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
	panic("unported: mj-core/src/codex_catalog.rs entry_from_openai_model")
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
	panic("unported: mj-core/src/codex_catalog.rs merge_overrides")
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
	panic("unported: mj-core/src/codex_catalog.rs guardian_review_model")
}

// versionOf is the dotted number sequence in a slug, as numbers.
// `glm-5.3-flash` is `[5, 3]`; a slug with no numbers is an empty vector,
// which sorts first.
func versionOf(slug string) []uint64 {
	panic("unported: mj-core/src/codex_catalog.rs version_of")
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
