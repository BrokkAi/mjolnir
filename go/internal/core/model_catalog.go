// Port of mj-core/src/model_catalog.rs.

// Shared interpretation of models advertised by harness profiles. Identity
// names the model version; an offer retains the harness launch value.

package core

import (
	"strings"
)

func isClaudeFamily(value string) bool {
	switch value {
	case "opus", "sonnet", "haiku", "fable", "mythos":
		return true
	}
	return false
}

func contextHint(value string) (string, bool) {
	rest, ok := strings.CutSuffix(value, "]")
	if !ok {
		return "", false
	}
	index := strings.LastIndexByte(rest, '[')
	if index < 0 {
		return "", false
	}
	return rest[index+1:], true
}

// familyVersion is the family and version of a "<Family> <version>" display
// name such as "Opus 5.5" or "Claude Opus 5.5 (1M context)".
func familyVersion(choice *SessionConfigChoice) (string, []uint32, bool) {
	panic("unported: mj-core/src/model_catalog.rs family_version")
}

// ChoiceModelID is the model identity a harness says an advertised choice
// runs. Context hints remain part of selection identity; pricing can discard
// them later.
func ChoiceModelID(choice *SessionConfigChoice) string {
	panic("unported: mj-core/src/model_catalog.rs choice_model_id")
}

// ModelID resolves a request once against all available advertised catalogs.
// If profiles disagree about a moving alias, its newest advertised version
// wins. Unknown values remain opaque; explicit versions never follow another
// version.
func ModelID(choices []SessionConfigChoice, wanted string) string {
	panic("unported: mj-core/src/model_catalog.rs model_id")
}

// ModelCatalogModelOffer is Rust's `model_catalog::ModelOffer` (prefixed:
// `ModelOffer` is config/harness's name): one identity and the exact
// advertised value to use with a profile's harness.
type ModelCatalogModelOffer struct {
	Identity string
	Value    string
}

// Offer finds a profile's launch value for an already resolved identity.
// Prefer the requested spelling when offered, then the identity itself, then
// a stable value.
func Offer(choices []SessionConfigChoice, identity, preferred string) (ModelCatalogModelOffer, bool) {
	panic("unported: mj-core/src/model_catalog.rs offer")
}

// ModelCatalogResolve is Rust's `model_catalog::resolve` (prefixed: the name
// is login_environment's): resolve a request and find its advertised launch
// value in one catalog.
func ModelCatalogResolve(choices []SessionConfigChoice, wanted string) (ModelCatalogModelOffer, bool) {
	return Offer(choices, ModelID(choices, wanted), wanted)
}

// MergedChoices projects combined catalogs to one choice per identity. A
// moving alias naming an older version becomes that explicit identity, so two
// profiles' different versions remain selectable instead of sharing one
// option value.
func MergedChoices(choices []SessionConfigChoice) []SessionConfigChoice {
	panic("unported: mj-core/src/model_catalog.rs merged_choices")
}

// NormalizeModel normalizes provider qualifiers without losing versions or
// context requirements.
func NormalizeModel(model string) string {
	model = modelCatalogASCIILower(model)
	// Known provider qualifiers name the same underlying model.
	model = modelCatalogCutFirstPrefix(model, "global.", "us.", "eu.")
	model = modelCatalogCutFirstPrefix(model, "openai.", "anthropic.")
	model = modelCatalogCutFirstPrefix(model, "openai/", "anthropic/", "z-ai/", "deepseek/", "x-ai/", "moonshotai/")
	return strings.TrimSuffix(model, ":0")
}

// modelCatalogCutFirstPrefix removes the first of prefixes that value starts
// with.
func modelCatalogCutFirstPrefix(value string, prefixes ...string) string {
	for _, candidate := range prefixes {
		if rest, ok := strings.CutPrefix(value, candidate); ok {
			return rest
		}
	}
	return value
}

// modelCatalogASCIILower is Rust's `to_ascii_lowercase`: only ASCII letters
// change.
func modelCatalogASCIILower(value string) string {
	lowered := []byte(value)
	for i, c := range lowered {
		if 'A' <= c && c <= 'Z' {
			lowered[i] = c + ('a' - 'A')
		}
	}
	return string(lowered)
}

// ModelVersionCmp is shared with utility inference: moving aliases precede
// numbered versions. It returns -1, 0 or 1.
func ModelVersionCmp(left, right string) int {
	panic("unported: mj-core/src/model_catalog.rs model_version_cmp")
}
