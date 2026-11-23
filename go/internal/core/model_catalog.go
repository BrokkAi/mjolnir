// Port of mj-core/src/model_catalog.rs.

// Shared interpretation of models advertised by harness profiles. Identity
// names the model version; an offer retains the harness launch value.

package core

import (
	"cmp"
	"slices"
	"strconv"
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
	name := strings.TrimSpace(choice.Name)
	name = strings.TrimPrefix(name, "Claude ")
	if index := strings.LastIndex(name, " ("); index >= 0 && strings.HasSuffix(name[index+2:], " context)") {
		name = name[:index]
	}
	family, version, ok := strings.Cut(name, " ")
	if !ok {
		return "", nil, false
	}
	if family == "" {
		return "", nil, false
	}
	for _, b := range []byte(family) {
		if !(b >= 'a' && b <= 'z' || b >= 'A' && b <= 'Z') {
			return "", nil, false
		}
	}
	var numbers []uint32
	for _, part := range strings.Split(version, ".") {
		for _, b := range []byte(part) {
			if b < '0' || b > '9' {
				return "", nil, false
			}
		}
		parsed, err := strconv.ParseUint(part, 10, 32)
		if err != nil {
			return "", nil, false
		}
		numbers = append(numbers, uint32(parsed))
	}
	return family, numbers, true
}

// ChoiceModelID is the model identity a harness says an advertised choice
// runs. Context hints remain part of selection identity; pricing can discard
// them later.
func ChoiceModelID(choice *SessionConfigChoice) string {
	value := NormalizeModel(choice.Value)
	base, _, _ := strings.Cut(value, "[")
	if isClaudeFamily(base) {
		if family, version, ok := familyVersion(choice); ok && modelCatalogEqualFold(family, base) {
			parts := make([]string, len(version))
			for i, number := range version {
				parts[i] = strconv.FormatUint(uint64(number), 10)
			}
			hint := ""
			if text, ok := contextHint(value); ok {
				hint = "[" + text + "]"
			}
			return "claude-" + modelCatalogASCIILower(family) + "-" + strings.Join(parts, "-") + hint
		}
	}
	return value
}

// ModelID resolves a request once against all available advertised catalogs.
// If profiles disagree about a moving alias, its newest advertised version
// wins. Unknown values remain opaque; explicit versions never follow another
// version.
func ModelID(choices []SessionConfigChoice, wanted string) string {
	wanted = strings.TrimSpace(wanted)
	best, found := "", false
	for i := range choices {
		choice := &choices[i]
		if !modelCatalogEqualFold(choice.Value, wanted) && !modelCatalogEqualFold(choice.Name, wanted) {
			continue
		}
		id := ChoiceModelID(choice)
		// Rust's max_by keeps the last of equal maxima.
		if !found || ModelVersionCmp(id, best) >= 0 {
			best, found = id, true
		}
	}
	if found {
		return best
	}
	if isClaudeFamily(modelCatalogASCIILower(wanted)) {
		var bestChoice *SessionConfigChoice
		var bestVersion []uint32
		for i := range choices {
			choice := &choices[i]
			family, version, ok := familyVersion(choice)
			if !ok || !modelCatalogEqualFold(family, wanted) {
				continue
			}
			if bestChoice == nil {
				bestChoice, bestVersion = choice, version
				continue
			}
			// Compare (best, candidate) as Rust's max_by(left, right) does.
			left, right := bestChoice, choice
			order := slices.Compare(bestVersion, version)
			if order == 0 {
				_, rightHint := contextHint(right.Value)
				_, leftHint := contextHint(left.Value)
				order = modelCatalogBoolCmp(rightHint, leftHint)
			}
			if order == 0 {
				order = cmp.Compare(left.Value, right.Value)
			}
			if order <= 0 {
				bestChoice, bestVersion = choice, version
			}
		}
		if bestChoice != nil {
			return ChoiceModelID(bestChoice)
		}
	}
	return NormalizeModel(wanted)
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
	var best *SessionConfigChoice
	key := func(choice *SessionConfigChoice) (bool, bool, string) {
		return choice.Value != preferred, choice.Value != identity, choice.Value
	}
	for i := range choices {
		choice := &choices[i]
		if ChoiceModelID(choice) != identity {
			continue
		}
		if best == nil {
			best = choice
			continue
		}
		a1, a2, a3 := key(choice)
		b1, b2, b3 := key(best)
		order := modelCatalogBoolCmp(a1, b1)
		if order == 0 {
			order = modelCatalogBoolCmp(a2, b2)
		}
		if order == 0 {
			order = cmp.Compare(a3, b3)
		}
		// Rust's min_by_key keeps the first of equal minima.
		if order < 0 {
			best = choice
		}
	}
	if best == nil {
		return ModelCatalogModelOffer{}, false
	}
	return ModelCatalogModelOffer{Identity: identity, Value: best.Value}, true
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
	catalogs := slices.Clone(choices)
	seen := map[string]bool{}
	merged := []SessionConfigChoice{}
	for i := range catalogs {
		choice := &catalogs[i]
		identity := ChoiceModelID(choice)
		if seen[identity] {
			continue
		}
		seen[identity] = true
		projected := *choice
		if projected.Description != nil {
			description := *projected.Description
			projected.Description = &description
		}
		if ModelID(catalogs, choice.Value) != identity {
			projected.Value = identity
		}
		merged = append(merged, projected)
	}
	return merged
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

// ModelProvider maps a model name or identity to its owning QuotaProvider,
// if it belongs to a known model family.
func ModelProvider(model string) QuotaProvider {
	normalized := NormalizeModel(strings.TrimSpace(model))
	base, _, _ := strings.Cut(normalized, "[")
	base = strings.TrimSpace(base)
	if base == "" {
		return nil
	}
	startsWithToken := func(prefix string) bool {
		rest, ok := strings.CutPrefix(base, prefix)
		return ok && (rest == "" || strings.ContainsRune("-_./", rune(rest[0])))
	}
	hasPrefix := func(prefixes ...string) bool {
		for _, prefix := range prefixes {
			if strings.HasPrefix(base, prefix) {
				return true
			}
		}
		return false
	}
	switch {
	case hasPrefix("claude", "opus", "sonnet", "haiku", "fable", "mythos"):
		return QuotaProviderAnthropic{}
	case hasPrefix("gpt") || startsWithToken("o1") || startsWithToken("o3") || startsWithToken("o4"):
		return QuotaProviderOpenAI{}
	case hasPrefix("gemini", "argon", "g1-"):
		return QuotaProviderGoogle{}
	case hasPrefix("grok"):
		return QuotaProviderXAI{}
	case hasPrefix("kimi") || startsWithToken("k2") || startsWithToken("k3"):
		return QuotaProviderMoonshot{}
	case hasPrefix("deepseek"):
		return QuotaProviderDeepSeek{}
	case hasPrefix("glm"):
		return QuotaProviderZai{}
	case hasPrefix("muse"):
		return QuotaProviderMeta{}
	}
	return nil
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

// modelCatalogEqualFold is Rust's `eq_ignore_ascii_case`.
func modelCatalogEqualFold(left, right string) bool {
	return len(left) == len(right) && modelCatalogASCIILower(left) == modelCatalogASCIILower(right)
}

// modelCatalogBoolCmp orders false before true, as Rust's bool Ord does.
func modelCatalogBoolCmp(left, right bool) int {
	switch {
	case left == right:
		return 0
	case !left:
		return -1
	}
	return 1
}

// ModelVersionCmp is shared with utility inference: moving aliases precede
// numbered versions. It returns -1, 0 or 1.
func ModelVersionCmp(left, right string) int {
	alias := func(id string) int {
		for _, part := range strings.FieldsFunc(id, func(r rune) bool { return r == '-' || r == '_' || r == '.' }) {
			if part == "latest" || part == "next" {
				return 1
			}
		}
		return 0
	}
	numbers := func(id string) []uint64 {
		var found []uint64
		// A context capacity such as K3's `-256k` is not a release version.
		for _, part := range strings.FieldsFunc(id, func(r rune) bool { return r == '-' || r == '_' || r == '.' || r == '/' }) {
			size, ok := strings.CutSuffix(part, "k")
			if !ok {
				size, ok = strings.CutSuffix(part, "m")
			}
			if ok && size != "" && strings.Trim(size, "0123456789") == "" {
				continue
			}
			for _, digits := range strings.FieldsFunc(part, func(r rune) bool { return r < '0' || r > '9' }) {
				if value, err := strconv.ParseUint(digits, 10, 64); err == nil {
					found = append(found, value)
				}
			}
		}
		return found
	}
	if order := cmp.Compare(alias(left), alias(right)); order != 0 {
		return order
	}
	if order := slices.Compare(numbers(left), numbers(right)); order != 0 {
		return order
	}
	// Highspeed is a same-version speed variant, not a newer model.
	if order := modelCatalogBoolCmp(strings.HasSuffix(right, "-highspeed"), strings.HasSuffix(left, "-highspeed")); order != 0 {
		return order
	}
	return cmp.Compare(left, right)
}
