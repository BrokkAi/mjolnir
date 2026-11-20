// Port of mj-core/src/usage/model_catalog.rs.
//
// Price recorded model selections through their profile's advertised catalog.

package core

// ModelCatalogs is the newest discovered model list of each profile.
type ModelCatalogs map[string][]SessionConfigChoice

// PricedModel is the model id to price model as: itself when it has a rate,
// else the priced model its profile's catalog says it runs, else itself.
func (c ModelCatalogs) PricedModel(profile *string, model string) string {
	if IsPriced(model) || profile == nil {
		return model
	}
	choices, ok := c[*profile]
	if !ok {
		return model
	}
	if id := UsagePricingNormalizeModel(ModelID(choices, model)); IsPriced(id) {
		return id
	}
	return model
}
