# `go/crates/clap` for clap 4.6.1 (`clap_builder` 4.6.0, `clap_derive`)

Oracle module `go/port/oracle/src/clap_oracle.rs`, feature `clap`.

Mjolnir's programs define their command lines with clap's derive macros: `mj`
in `mj-cli/src/` and `mj-worker` in `mj-worker-bin/src/main.rs` and
`mj-worker/src/` (start with `grep -rn 'derive(.*\(Parser\|Subcommand\|Args\|ValueEnum\)'`).
The Go programs must accept the same command lines and print the same help,
usage, version and error text; there are goldens in `mj-cli/tests/golden/`
and `mj-worker-bin/tests/golden/`. This is a port of clap's observable
behaviour for everything those definitions use, from the source
(`clap_builder-4.6.0/src/`, `clap_derive-4.6.1/src/`, `clap_lex`, `anstyle`,
`anstream`).

1. Inventory every attribute and API used into `doc.go` (`command`, `arg`,
   `group`, `value` attributes with each option: `long`, `short`,
   `value_name`, defaults, `num_args`, delimiters, every `ArgAction`, `env`,
   `hide`, `global`, conflicts and requirements, every `value_parser` kind,
   trailing and hyphen values, `last`, aliases, subcommand settings,
   `flatten`, doc comments as help, about texts, version propagation, help
   templates and headings, `display_order`, `styles`, value enums, `Option<Option<T>>`,
   `Vec<T>`, counts, external subcommands, and manual `Command`,
   `CommandFactory`, `ArgMatches` and `Error` use).
2. The builder engine with clap's exact semantics for the inventory: lexing
   (`--long=value`, `-abc`, `-ovalue`, `--`, negative numbers, hyphen values);
   parsing (subcommands, positionals, defaults and environment precedence and
   `ValueSource`, flags and counts, validation in clap's order, help and
   version at every level, the `help` subcommand); every error message with
   its context, `tip:` suggestions (Jaro similarity, threshold 0.7), usage
   line and `For more information, try '--help'.`, exit codes and streams;
   usage generation; help rendering (default template, `-h` vs `--help`,
   alignment, the next-line decision, wrapping at the terminal width with the
   same environment handling, possible values, defaults and env annotations,
   hidden items, subcommand lists); styling (when ANSI is emitted, `NO_COLOR`,
   `CLICOLOR`, `CLICOLOR_FORCE`, `TERM`, tty, the exact sequences); `--version`.
3. The Go spelling of a derive definition, designed so `mj-cli` can be
   transcribed mechanically: the ported struct keeps its fields, a function
   builds the `clap.Command` with builder calls in attribute order, and a
   function fills the struct from `ArgMatches` as `FromArgMatches` does.
   Document it in `doc.go` with one complete example taken from the real
   `mj-worker` definition, tested against that program's golden help.

Tests: clap's own builder and derive tests that apply, and differential tests:
commands exercising the inventory in the oracle (including a transcription of
`mj-worker`'s real definition and a representative slice of `mj`'s), built the
same in Go, compared for a large generated set of argument vectors (valid,
invalid in every way, help and version at each level, near-miss names, various
widths and colour settings) on parsed result, stdout, stderr and exit code.
