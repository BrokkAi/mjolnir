// Port of clap_builder 4.6.0 src/derive.rs
//
// This file contains traits that are user implemented, or derived with
// clap_derive. doc.go describes how a derive is spelled in Go.

package clap

import "github.com/BrokkAi/mjolnir/go/rs"

// Parser is clap::Parser: parse command-line arguments into `Self`.
//
// The primary one-stop-shop trait used to create an instance of a `clap`
// Command, conduct the parsing, and turn the resulting ArgMatches back into
// concrete instance of the user struct.
//
// This trait is primarily a convenience on top of FromArgMatches +
// CommandFactory which uses those two underlying traits to build the two
// fundamental functions `parse` which uses the `std::env::args_os`
// iterator, and `parse_from` which allows the consumer to supply the
// iterator (along with fallible options for each).
//
// In Go a derived Parser is a value holding the type's CommandFactory and
// FromArgMatches functions, which the derive transcription writes as
// package-level functions (see doc.go); its methods are the trait's
// provided methods. For `#[derive(Parser)] struct Cli` in `main.rs`:
//
//	var main_Cli_Parser = clap.Parser[main_Cli]{
//		Command:           main_Cli_command,
//		FromArgMatchesMut: main_Cli_from_arg_matches_mut,
//	}
//
// and `Cli::parse()` is `main_Cli_Parser.Parse()`. CommandForUpdate and
// UpdateFromArgMatchesMut are needed only by UpdateFrom and TryUpdateFrom.
type Parser[T any] struct {
	// Command is CommandFactory::command: build a Command that can
	// instantiate `Self`.
	Command func() *Command
	// CommandForUpdate is CommandFactory::command_for_update: build a
	// Command that can update `self`.
	CommandForUpdate func() *Command
	// FromArgMatchesMut is FromArgMatches::from_arg_matches_mut:
	// instantiate `Self` from ArgMatches, parsing the arguments as needed.
	FromArgMatchesMut func(matches *ArgMatches) (T, error)
	// UpdateFromArgMatchesMut is FromArgMatches::update_from_arg_matches_mut:
	// assign values from `ArgMatches` to `self`.
	UpdateFromArgMatchesMut func(self *T, matches *ArgMatches) error
}

// Parse is Parser::parse: parse from `std::env::args_os()`, exit on error.
func (p Parser[T]) Parse() T {
	matches := p.Command().GetMatches()
	res, err := p.FromArgMatchesMut(matches)
	if err != nil {
		// Since this is more of a development-time error, we aren't doing as fancy of a quit
		// as `get_matches`
		formatError(p.Command, err).Exit()
	}
	return res
}

// TryParse is Parser::try_parse: parse from `std::env::args_os()`, return
// Err on error.
func (p Parser[T]) TryParse() (T, error) {
	var zero T
	matches, err := p.Command().TryGetMatches()
	if err != nil {
		return zero, err
	}
	res, err := p.FromArgMatchesMut(matches)
	if err != nil {
		return zero, formatError(p.Command, err)
	}
	return res, nil
}

// ParseFrom is Parser::parse_from: parse from iterator, exit on error.
func (p Parser[T]) ParseFrom(itr []string) T {
	matches := p.Command().GetMatchesFrom(itr)
	res, err := p.FromArgMatchesMut(matches)
	if err != nil {
		// Since this is more of a development-time error, we aren't doing as fancy of a quit
		// as `get_matches_from`
		formatError(p.Command, err).Exit()
	}
	return res
}

// TryParseFrom is Parser::try_parse_from: parse from iterator, return Err
// on error.
func (p Parser[T]) TryParseFrom(itr []string) (T, error) {
	var zero T
	matches, err := p.Command().TryGetMatchesFrom(itr)
	if err != nil {
		return zero, err
	}
	res, err := p.FromArgMatchesMut(matches)
	if err != nil {
		return zero, formatError(p.Command, err)
	}
	return res, nil
}

// UpdateFrom is Parser::update_from: update from iterator, exit on error.
func (p Parser[T]) UpdateFrom(self *T, itr []string) {
	matches := p.CommandForUpdate().GetMatchesFrom(itr)
	if err := p.UpdateFromArgMatchesMut(self, matches); err != nil {
		// Since this is more of a development-time error, we aren't doing as fancy of a quit
		// as `get_matches_from`
		formatError(p.Command, err).Exit()
	}
}

// TryUpdateFrom is Parser::try_update_from: update from iterator, return
// Err on error.
func (p Parser[T]) TryUpdateFrom(self *T, itr []string) error {
	matches, err := p.CommandForUpdate().TryGetMatchesFrom(itr)
	if err != nil {
		return err
	}
	if err := p.UpdateFromArgMatchesMut(self, matches); err != nil {
		return formatError(p.Command, err)
	}
	return nil
}

// formatError is `format_error::<Self>`: the error formatted with the
// type's Command.
func formatError(command func() *Command, err error) *Error {
	cmd := command()
	return errorFromAny(err).Format(cmd)
}

// ValueEnum is clap::ValueEnum: parse arguments into enums.
//
// When deriving Parser, a field whose type implements ValueEnum can have
// the attribute `#[arg(value_enum)]` which will
//   - Call EnumValueParser
//   - Allowing using the `#[arg(default_value_t)]` attribute without
//     implementing `Display`.
//
// A data-free Rust enum is `type E uint8` in Go, so the trait's functions
// are methods of the value: ValueVariants ignores its receiver.
type ValueEnum[E any] interface {
	// ValueVariants is ValueEnum::value_variants: all possible argument
	// values, in display order.
	ValueVariants() []E
	// ToPossibleValue is ValueEnum::to_possible_value: the canonical
	// argument value.
	//
	// The value is `None` for skipped variants.
	ToPossibleValue() rs.Option[BuilderPossibleValue]
}

// ValueEnumFromStr is the default body of ValueEnum::from_str: parse an
// argument into `Self`.
func ValueEnumFromStr[E ValueEnum[E]](input string, ignoreCase bool) (E, error) {
	var zero E
	for _, v := range zero.ValueVariants() {
		pv := v.ToPossibleValue().Expect("ValueEnum::value_variants contains only values with a corresponding ValueEnum::to_possible_value")
		if pv.Matches(input, ignoreCase) {
			return v, nil
		}
	}
	return zero, stringError("invalid variant: " + input)
}
