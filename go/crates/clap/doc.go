// Package clap stands in for the Rust crate clap 4.6.1 (clap_builder 4.6.0,
// clap_derive 4.6.1, clap_lex 1.1.0, anstyle 1.0.14, anstream 1.0.0,
// anstyle-query 1.1.5, strsim 0.11.1), built with the features Mjolnir's
// Cargo.lock resolves: the defaults (std, color, help, usage,
// error-context, suggestions) plus derive and env. Not wrap_help: help is
// never wrapped and the terminal width is never asked for, so help renders
// for a 100-column terminal unless Command::term_width says otherwise. Not
// unicode: every character is one column wide.
//
// This doc comment is being written; the complete inventory and the Go
// spelling of a derive follow in a later revision of this file.
package clap
