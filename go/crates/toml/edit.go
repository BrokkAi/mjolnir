// Port of toml 0.8.2 src/edit.rs

package toml

// With the `parse` and `display` features on (Mjolnir builds toml with its
// default features), edit.rs only brings toml_edit's error types into the
// crate:
//
//	pub(crate) mod de {
//	    pub(crate) use toml_edit::de::Error;
//	}
//
//	pub(crate) mod ser {
//	    pub(crate) use toml_edit::ser::Error;
//	}
//
// de.go and ser.go name tomledit.DeError and tomledit.SerError directly.
