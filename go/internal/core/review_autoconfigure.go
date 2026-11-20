// Port of mj-core/src/review/autoconfigure.rs.

// Ordered reviewer defaults from advertised enabled-profile models.

package core

import (
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
	panic("unported: mj-core/src/review/autoconfigure.rs fallback_effort")
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
	panic("unported: mj-core/src/review/autoconfigure.rs model_offers")
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
	panic("unported: mj-core/src/review/autoconfigure.rs offers_drafted_family")
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
	panic("unported: mj-core/src/review/autoconfigure.rs autoconfigure")
}
