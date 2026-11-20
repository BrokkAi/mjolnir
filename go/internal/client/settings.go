// Port of mj-client/src/settings/mod.rs.

// The settings model every interface shares: which settings exist, how a
// page lists them, what they are called and set to, how an edit changes the
// draft, and how a draft is saved. Surfaces own only their own drawing.
//
// The modules are the settings_*.go files of this package. A settings draft
// is a serde_json::Value, held here in the form of
// github.com/BrokkAi/acp-go/jsonvalue (nil, bool, string, int64, uint64,
// float64, []any and jsonvalue.Object, whose members keep their order as
// serde_json's preserve_order does). A `&mut Value` parameter is a *any.

package client
