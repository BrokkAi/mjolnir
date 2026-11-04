//! The settings model every interface shares: which settings exist, how a
//! page lists them, what they are called and set to, how an edit changes the
//! draft, and how a draft is saved. Surfaces own only their own drawing.
pub mod discovery;
pub mod draft;
pub mod edit;
pub mod hidden;
pub mod page;
pub mod save;
pub mod schema;
pub mod view;
