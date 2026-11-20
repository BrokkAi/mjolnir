// Port of mj-core/src/review/autoconfigure.rs.

// Ordered reviewer defaults from advertised enabled-profile models.

package core

import (
	"cmp"
	"maps"
	"slices"
	"strings"
)

type EffortAdjustment struct {
	Model     string
	Requested string
	Used      *string
}

type Autoconfigured struct {
	Reviewers []SubagentChoice
	Adjusted  []EffortAdjustment
}

// FallbackEffort uses the closest offered effort below the requested one, or
// the lowest offered effort if every choice is higher.
func FallbackEffort(requested string, choices []SessionConfigChoice) (string, bool) {
	rank := func(value string) int {
		return slices.Index([]string{"minimal", "low", "medium", "high", "xhigh", "max"}, value)
	}
	if slices.ContainsFunc(choices, func(choice SessionConfigChoice) bool { return choice.Value == requested }) {
		return requested, true
	}
	requestedRank := rank(requested)
	if requestedRank < 0 {
		return "", false
	}
	// Rust's max_by_key keeps the last of equal ranks, min_by_key the first.
	below, belowRank := "", -1
	lowest, lowestRank := "", -1
	for _, choice := range choices {
		choiceRank := rank(choice.Value)
		if choiceRank < 0 {
			continue
		}
		if choiceRank < requestedRank && choiceRank >= belowRank {
			below, belowRank = choice.Value, choiceRank
		}
		if lowestRank < 0 || choiceRank < lowestRank {
			lowest, lowestRank = choice.Value, choiceRank
		}
	}
	if belowRank >= 0 {
		return below, true
	}
	if lowestRank >= 0 {
		return lowest, true
	}
	return "", false
}

// AutoconfigureModelOffer is one advertised model value and which enabled
// profiles offer it. (Rust: `ModelOffer`, prefixed for the collision.)
type AutoconfigureModelOffer struct {
	Choice   SessionConfigChoice
	Identity string
	// Profiles is the enabled profiles offering this model identity.
	Profiles int
	// SubscriptionProfiles is those profiles that are not usage-priced. A
	// profile whose billing is unknown counts as a subscription.
	SubscriptionProfiles int
}

// AutoconfigureProfileModels is one enabled profile's advertised models and whether it
// is usage-priced. (Rust passes `(&[SessionConfigChoice], bool)` pairs.)
type AutoconfigureProfileModels struct {
	Models      []SessionConfigChoice
	UsagePriced bool
}

// ModelOffers merges each enabled profile's advertised models, flagged
// usage-priced or not, into one offer per distinct identity, in first-seen
// order.
func ModelOffers(profiles []AutoconfigureProfileModels) []AutoconfigureModelOffer {
	offers := []AutoconfigureModelOffer{}
	for _, profile := range profiles {
		seen := map[string]bool{}
		for _, choice := range profile.Models {
			identity := ChoiceModelID(&choice)
			if seen[identity] {
				continue
			}
			seen[identity] = true
			index := slices.IndexFunc(offers, func(offer AutoconfigureModelOffer) bool { return offer.Identity == identity })
			if index < 0 {
				offers = append(offers, AutoconfigureModelOffer{Choice: choice, Identity: identity})
				index = len(offers) - 1
			}
			offers[index].Profiles++
			if !profile.UsagePriced {
				offers[index].SubscriptionProfiles++
			}
		}
	}
	choices := make([]SessionConfigChoice, 0, len(offers))
	for _, offer := range offers {
		choices = append(choices, offer.Choice)
	}
	for index, choice := range MergedChoices(choices) {
		if index >= len(offers) {
			break
		}
		offers[index].Choice = choice
	}
	return offers
}

// MergedModelOffers is offers from a merged model list whose sources are
// unknown: each model counts as offered by one subscription profile. Enough
// to ask whether autoconfigure has any answer; not to rank values by where
// they come from.
func MergedModelOffers(models []SessionConfigChoice) []AutoconfigureModelOffer {
	return ModelOffers([]AutoconfigureProfileModels{{Models: models, UsagePriced: false}})
}

// OffersDraftedFamily reports whether profileModels includes a model in a
// family that drafted reviewers use. all is every profile's models merged,
// which names the identities the families are read from.
func OffersDraftedFamily(profileModels, all []SessionConfigChoice, drafted []SubagentChoice) bool {
	family := func(value string) (ReviewModelFamily, bool) {
		return ReviewModelFamilyForModel(ModelID(all, value))
	}
	var draftedFamilies []ReviewModelFamily
	for _, choice := range drafted {
		if found, ok := family(choice.Model); ok {
			draftedFamilies = append(draftedFamilies, found)
		}
	}
	for _, model := range profileModels {
		if found, ok := family(model.Value); ok && slices.Contains(draftedFamilies, found) {
			return true
		}
	}
	return false
}

// agentCompareBool orders false before true, as Rust's bool Ord does.
func agentCompareBool(left, right bool) int {
	switch {
	case left == right:
		return 0
	case left:
		return 1
	}
	return -1
}

// undated is id without a trailing `-YYYYMMDD` snapshot date.
func undated(id string) string {
	i := strings.LastIndexByte(id, '-')
	if i < 0 {
		return id
	}
	date := id[i+1:]
	if len(date) == 8 && strings.Trim(date, "0123456789") == "" {
		return id[:i]
	}
	return id
}

// fullModel: full models precede reduced variants even when the latter are
// newer or offered by more profiles. Opaque advertised models remain
// candidates.
func fullModel(id string) bool {
	for _, part := range strings.FieldsFunc(id, func(r rune) bool {
		return r == '-' || r == '_' || r == '.' || r == '/'
	}) {
		switch part {
		case "flash", "flashx", "mini", "nano", "lite", "small", "haiku",
			"luna", "dsf", "dsfmax", "default", "auto":
			return false
		}
	}
	return true
}

// Autoconfigure chooses review slots from the enabled profiles' model
// offers. Within a family full models precede reduced variants, then a
// subscription value beats one only usage-priced profiles offer, then the
// value offered by more enabled profiles wins, then the newest version.
// Reviewers are load-balanced only across profiles that offer the same
// resolved identity, so this keeps reviews on subscriptions and spread over
// as many profiles as possible. efforts returns a model's offered efforts,
// or false when unknown.
func Autoconfigure(offers []AutoconfigureModelOffer, efforts func(model string) ([]SessionConfigChoice, bool)) (Autoconfigured, bool) {
	// Each family keeps its best (offer, identity) pair; identities decide
	// versions, values are what the reviewer is launched with.
	type pick struct {
		offer *AutoconfigureModelOffer
		id    string
	}
	selected := map[ReviewModelFamily]pick{}
	subscribed := func(offer *AutoconfigureModelOffer) bool { return offer.SubscriptionProfiles > 0 }
	for index := range offers {
		offer := &offers[index]
		id := offer.Identity
		family, ok := ReviewModelFamilyForModel(id)
		if !ok {
			continue
		}
		better := true
		if old, exists := selected[family]; exists {
			switch {
			case fullModel(id) != fullModel(old.id):
				better = fullModel(id)
			case subscribed(offer) != subscribed(old.offer):
				better = subscribed(offer)
			case offer.Profiles != old.offer.Profiles:
				better = offer.Profiles > old.offer.Profiles
			default:
				// A dated snapshot is the same version as its undated alias,
				// which follows the newest snapshot, so the alias wins a tie.
				newBase, oldBase := undated(id), undated(old.id)
				order := ModelVersionCmp(newBase, oldBase)
				if order == 0 {
					order = agentCompareBool(len(newBase) == len(id), len(oldBase) == len(old.id))
				}
				better = order > 0
			}
		}
		if better {
			selected[family] = pick{offer: offer, id: id}
		}
	}
	var adjusted []EffortAdjustment
	choice := func(family ReviewModelFamily, effort string) SubagentChoice {
		model := selected[family].offer.Choice.Value
		used := &effort
		if offered, known := efforts(model); known {
			used = nil
			if fallback, ok := FallbackEffort(effort, offered); ok {
				used = &fallback
			}
		}
		if used == nil || *used != effort {
			adjusted = append(adjusted, EffortAdjustment{Model: model, Requested: effort, Used: used})
		}
		return SubagentChoice{Model: model, Effort: used}
	}
	has := func(kind ReviewModelFamilyKind) bool {
		_, ok := selected[ReviewModelFamily{Kind: kind}]
		return ok
	}
	type slot struct {
		family ReviewModelFamily
		effort string
	}
	var preset [2]slot
	if has(ReviewModelFamilyTerra) && has(ReviewModelFamilySol) {
		preset = [2]slot{
			{ReviewModelFamily{Kind: ReviewModelFamilyTerra}, "high"},
			{ReviewModelFamily{Kind: ReviewModelFamilySol}, "medium"},
		}
	} else {
		var chosen *ReviewModelFamily
		for _, kind := range []ReviewModelFamilyKind{ReviewModelFamilySonnet, ReviewModelFamilyDeepSeek, ReviewModelFamilyGrok, ReviewModelFamilyKimi} {
			if has(kind) {
				chosen = &ReviewModelFamily{Kind: kind}
				break
			}
		}
		if chosen == nil {
			families := slices.SortedFunc(maps.Keys(selected), ReviewModelFamily.Compare)
			for index := range families {
				family := families[index]
				if chosen == nil {
					chosen = &family
					continue
				}
				candidate, best := selected[family], selected[*chosen]
				order := agentCompareBool(fullModel(candidate.id), fullModel(best.id))
				if order == 0 {
					order = agentCompareBool(subscribed(candidate.offer), subscribed(best.offer))
				}
				if order == 0 {
					order = cmp.Compare(candidate.offer.Profiles, best.offer.Profiles)
				}
				if order == 0 {
					// Provider versions are incomparable; names only break
					// ties between otherwise equal providers.
					order = chosen.Compare(family)
				}
				// Rust's max_by keeps the last of equal elements.
				if order >= 0 {
					chosen = &family
				}
			}
		}
		if chosen == nil {
			return Autoconfigured{}, false
		}
		preset = [2]slot{{*chosen, "high"}, {*chosen, "high"}}
	}
	reviewers := make([]SubagentChoice, 0, len(preset))
	for _, slot := range preset {
		reviewers = append(reviewers, choice(slot.family, slot.effort))
	}
	if adjusted == nil {
		adjusted = []EffortAdjustment{}
	}
	return Autoconfigured{Reviewers: reviewers, Adjusted: adjusted}, true
}
