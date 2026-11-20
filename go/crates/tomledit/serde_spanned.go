// Port of serde_spanned 0.6.9 src/spanned.rs (the serde names toml_edit's de
// module recognises: `serde_spanned::__unstable`).

package tomledit

import "slices"

// Currently serde itself doesn't have a spanned type, so we map our `Spanned`
// to a special value in the serde data model. Namely one with these special
// fields/struct names.
//
// In general, supported deserializers should catch this and not literally emit
// these strings but rather emit `Spanned` as they're intended.
const (
	// SpannedName is serde_spanned::__unstable::NAME.
	SpannedName = "$__serde_spanned_private_Spanned"
	// SpannedStartField is serde_spanned::__unstable::START_FIELD.
	SpannedStartField = "$__serde_spanned_private_start"
	// SpannedEndField is serde_spanned::__unstable::END_FIELD.
	SpannedEndField = "$__serde_spanned_private_end"
	// SpannedValueField is serde_spanned::__unstable::VALUE_FIELD.
	SpannedValueField = "$__serde_spanned_private_value"
)

// SpannedIsSpanned is serde_spanned::__unstable::is_spanned: whether a
// deserialize_struct call is the one serde_spanned's `Spanned<T>` makes.
func SpannedIsSpanned(name string, fields []string) bool {
	return name == SpannedName && slices.Equal(fields, []string{SpannedStartField, SpannedEndField, SpannedValueField})
}
