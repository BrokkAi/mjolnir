// Port of clap_builder 4.6.0 src/parser/matches/arg_matches.rs

package clap

import (
	"reflect"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// ArgMatches is clap::ArgMatches: container for parse results.
//
// Used to get information about the arguments that were supplied to the
// program at runtime by the user. New instances of this struct are obtained
// by using the Command::get_matches family of methods.
//
// Rust's generic methods (`get_one::<T>`, `remove_many::<T>`, ...) are the
// package functions ArgMatchesGetOne, ArgMatchesRemoveMany and so on: a Go
// method cannot take a type argument. T is the Go type the argument's value
// parser produces: string for String, OsString and PathBuf.
type ArgMatches struct {
	// validArgs and validSubcommands are filled in debug builds only.
	validArgs        []Id
	validSubcommands []string
	args             utilFlatMap[Id, *parserMatchedArg]
	subcommand       *parserSubCommand
}

// # Arguments

// ArgMatchesGetOne is ArgMatches::get_one: gets the value of a specific
// option or positional argument.
//
// i.e. an argument that takes an additional value at runtime.
//
// Returns an error if the wrong type was used.
//
// Returns `None` if the option wasn't present.
//
// NOTE: This will always return `Some(value)` if default_value has been set.
// ArgMatches::value_source can be used to check if a value is present at
// runtime.
//
// Panics if the argument definition and access mismatch (debug builds). To
// handle this case programmatically, see ArgMatchesTryGetOne.
func ArgMatchesGetOne[T any](m *ArgMatches, id string) rs.Option[T] {
	value, err := argMatchesTryGetOne[T](m, id)
	return parserMatchesErrorUnwrap(id, value, err)
}

// GetCount is ArgMatches::get_count: gets the value of a specific
// ArgAction::Count flag.
//
// Panics if the argument's action is not ArgAction::Count.
func (m *ArgMatches) GetCount(id string) uint8 {
	value, ok := ArgMatchesGetOne[uint8](m, id).Get()
	if !ok {
		rs.PanicAt(1, "arg `"+id+"`'s `ArgAction` should be `Count` which should provide a default")
	}
	return value
}

// GetFlag is ArgMatches::get_flag: gets the value of a specific
// ArgAction::SetTrue or ArgAction::SetFalse flag.
//
// Panics if the argument's action is not ArgAction::SetTrue or
// ArgAction::SetFalse.
func (m *ArgMatches) GetFlag(id string) bool {
	value, ok := ArgMatchesGetOne[bool](m, id).Get()
	if !ok {
		rs.PanicAt(1, "arg `"+id+"`'s `ArgAction` should be one of `SetTrue`, `SetFalse` which should provide a default")
	}
	return value
}

// ArgMatchesGetMany is ArgMatches::get_many: iterate over values of a
// specific option or positional argument.
//
// i.e. an argument that takes multiple values at runtime.
//
// Returns an error if the wrong type was used.
//
// Returns `None` if the option wasn't present.
//
// Panics if the argument definition and access mismatch (debug builds). To
// handle this case programmatically, see ArgMatchesTryGetMany.
func ArgMatchesGetMany[T any](m *ArgMatches, id string) rs.Option[[]T] {
	value, err := argMatchesTryGetMany[T](m, id)
	return parserMatchesErrorUnwrap(id, value, err)
}

// ArgMatchesGetOccurrences is ArgMatches::get_occurrences: iterate over the
// values passed to each occurrence of an option.
//
// Each item is itself an iterator containing the arguments passed to a
// single occurrence of the option.
//
// If the option doesn't support multiple occurrences, or there was only a
// single occurrence, the iterator will only contain a single item.
//
// Returns `None` if the option wasn't present.
//
// Panics if the argument definition and access mismatch (debug builds). To
// handle this case programmatically, see ArgMatchesTryGetOccurrences.
func ArgMatchesGetOccurrences[T any](m *ArgMatches, id string) rs.Option[[][]T] {
	value, err := argMatchesTryGetOccurrences[T](m, id)
	return parserMatchesErrorUnwrap(id, value, err)
}

// GetRaw is ArgMatches::get_raw: iterate over the original argument values.
//
// An `OsStr` on Unix-like systems is any series of bytes, regardless of
// whether or not they contain valid UTF-8. Since Strings in Rust are
// guaranteed to be valid UTF-8, a valid filename on a Unix system as an
// argument value may contain invalid UTF-8.
//
// Returns `None` if the option wasn't present.
//
// Panics if the argument definition and access mismatch (debug builds). To
// handle this case programmatically, see ArgMatches::try_get_raw.
func (m *ArgMatches) GetRaw(id string) rs.Option[[]string] {
	value, err := m.tryGetRaw(id)
	return parserMatchesErrorUnwrap(id, value, err)
}

// GetRawOccurrences is ArgMatches::get_raw_occurrences: iterate over the
// original values for each occurrence of an option.
//
// Similar to ArgMatches::get_occurrences but returns raw values.
//
// An `OsStr` on Unix-like systems is any series of bytes, regardless of
// whether or not they contain valid UTF-8. Since Strings in Rust are
// guaranteed to be valid UTF-8, a valid filename on a Unix system as an
// argument value may contain invalid UTF-8.
//
// Returns `None` if the option wasn't present.
//
// Panics if the argument definition and access mismatch (debug builds). To
// handle this case programmatically, see ArgMatches::try_get_raw_occurrences.
func (m *ArgMatches) GetRawOccurrences(id string) rs.Option[[][]string] {
	value, err := m.tryGetRawOccurrences(id)
	return parserMatchesErrorUnwrap(id, value, err)
}

// ArgMatchesRemoveOne is ArgMatches::remove_one: returns the value of a
// specific option or positional argument.
//
// i.e. an argument that takes an additional value at runtime.
//
// Returns an error if the wrong type was used. No item will have been
// removed.
//
// Returns `None` if the option wasn't present.
//
// NOTE: This will always return `Some(value)` if default_value has been set.
// ArgMatches::value_source can be used to check if a value is present at
// runtime.
//
// Panics if the argument definition and access mismatch (debug builds). To
// handle this case programmatically, see ArgMatchesTryRemoveOne.
func ArgMatchesRemoveOne[T any](m *ArgMatches, id string) rs.Option[T] {
	value, err := argMatchesTryRemoveOne[T](m, id)
	return parserMatchesErrorUnwrap(id, value, err)
}

// ArgMatchesRemoveMany is ArgMatches::remove_many: return values of a
// specific option or positional argument.
//
// i.e. an argument that takes multiple values at runtime.
//
// Returns an error if the wrong type was used. No item will have been
// removed.
//
// Returns `None` if the option wasn't present.
//
// Panics if the argument definition and access mismatch (debug builds). To
// handle this case programmatically, see ArgMatchesTryRemoveMany.
func ArgMatchesRemoveMany[T any](m *ArgMatches, id string) rs.Option[[]T] {
	value, err := argMatchesTryRemoveMany[T](m, id)
	return parserMatchesErrorUnwrap(id, value, err)
}

// ArgMatchesRemoveOccurrences is ArgMatches::remove_occurrences: return
// values for each occurrence of an option.
//
// Each item is itself an iterator containing the arguments passed to a
// single occurrence of the option.
//
// If the option doesn't support multiple occurrences, or there was only a
// single occurrence, the iterator will only contain a single item.
//
// Returns `None` if the option wasn't present.
//
// Panics if the argument definition and access mismatch (debug builds). To
// handle this case programmatically, see ArgMatchesTryRemoveOccurrences.
func ArgMatchesRemoveOccurrences[T any](m *ArgMatches, id string) rs.Option[[][]T] {
	value, err := argMatchesTryRemoveOccurrences[T](m, id)
	return parserMatchesErrorUnwrap(id, value, err)
}

// ContainsId is ArgMatches::contains_id: check if values are present for
// the argument or group id
//
// NOTE: This will always return `true` if default_value has been set.
// ArgMatches::value_source can be used to check if a value is present at
// runtime.
//
// Panics if `id` is not a valid argument or group name (debug builds). To
// handle this case programmatically, see ArgMatches::try_contains_id.
func (m *ArgMatches) ContainsId(id string) bool {
	value, err := m.tryContainsId(id)
	return parserMatchesErrorUnwrap(id, value, err)
}

// Ids is ArgMatches::ids: iterate over Arg and ArgGroup Ids via
// ArgMatches::ids.
func (m *ArgMatches) Ids() []Id {
	return append([]Id{}, m.args.keys...)
}

// ArgsPresent is ArgMatches::args_present: check if any Arg were present
//
// See also Command::arg_required_else_help.
func (m *ArgMatches) ArgsPresent() bool {
	for _, v := range m.args.values {
		if source, ok := v.getSource().Get(); ok && source.isExplicit() {
			return true
		}
	}
	return false
}

// ValueSource is ArgMatches::value_source: report where argument value came
// from
//
// Panics if `id` is not a valid argument or group id (debug builds).
func (m *ArgMatches) ValueSource(id string) rs.Option[ParserValueSource] {
	value := m.getArg(id)
	if value == nil {
		return rs.None[ParserValueSource]()
	}
	return value.getSource()
}

// IndexOf is ArgMatches::index_of: the first index of that an argument
// showed up.
//
// Indices are similar to argv indices, but are not exactly 1:1.
//
// For flags (i.e. those arguments which don't have an associated value),
// indices refer to occurrence of the switch, such as `-f`, or `--flag`.
// However, for options the indices refer to the *values* `-o val` would
// therefore not represent two distinct indices, only the index for `val`
// would be recorded. This is by design.
//
// Besides the flag/option discrepancy, the primary difference between an
// argv index and clap index, is that clap continues counting once all
// arguments have properly separated, whereas an argv index does not.
//
// The examples should clear this up.
//
// NOTE: If an argument is allowed multiple times, this method will only give
// the *first* index. See ArgMatches::indices_of.
//
// Panics if `id` is not a valid argument or group id (debug builds).
func (m *ArgMatches) IndexOf(id string) rs.Option[int] {
	arg := m.getArg(id)
	if arg == nil {
		return rs.None[int]()
	}
	return arg.getIndex(0)
}

// IndicesOf is ArgMatches::indices_of: all indices an argument appeared at
// when parsing.
//
// Indices are similar to argv indices, but are not exactly 1:1.
//
// For flags (i.e. those arguments which don't have an associated value),
// indices refer to occurrence of the switch, such as `-f`, or `--flag`.
// However, for options the indices refer to the *values* `-o val` would
// therefore not represent two distinct indices, only the index for `val`
// would be recorded. This is by design.
//
// NOTE: For more information about how clap indices compared to argv
// indices, see ArgMatches::index_of
//
// Panics if `id` is not a valid argument or group id (debug builds).
func (m *ArgMatches) IndicesOf(id string) rs.Option[[]int] {
	arg := m.getArg(id)
	if arg == nil {
		return rs.None[[]int]()
	}
	// Indices is an iterator that yields `len` (the number of values) items
	// at most.
	return rs.Some(append([]int{}, arg.indices...))
}

// # Subcommands

// Subcommand is ArgMatches::subcommand: the name and `ArgMatches` of the
// current subcommand.
//
// # Subcommand values are put in a child ArgMatches
//
// Returns `None` if the subcommand wasn't present at runtime,
func (m *ArgMatches) Subcommand() rs.Option[rs.Tuple2[string, *ArgMatches]] {
	if m.subcommand == nil {
		return rs.None[rs.Tuple2[string, *ArgMatches]]()
	}
	return rs.Some(rs.T2(m.subcommand.name, m.subcommand.matches))
}

// RemoveSubcommand is ArgMatches::remove_subcommand: return the name and
// `ArgMatches` of the current subcommand.
//
// # Subcommand values are put in a child ArgMatches
//
// Returns `None` if the subcommand wasn't present at runtime,
func (m *ArgMatches) RemoveSubcommand() rs.Option[rs.Tuple2[string, *ArgMatches]] {
	sc := m.subcommand
	m.subcommand = nil
	if sc == nil {
		return rs.None[rs.Tuple2[string, *ArgMatches]]()
	}
	return rs.Some(rs.T2(sc.name, sc.matches))
}

// SubcommandMatches is ArgMatches::subcommand_matches: the `ArgMatches` for
// the current subcommand.
//
// # Subcommand values are put in a child ArgMatches
//
// Returns `None` if the subcommand wasn't present at runtime,
//
// Panics if `id` is not a valid subcommand (debug builds).
func (m *ArgMatches) SubcommandMatches(name string) rs.Option[*ArgMatches] {
	sc := m.getSubcommand(name)
	if sc == nil {
		return rs.None[*ArgMatches]()
	}
	return rs.Some(sc.matches)
}

// SubcommandName is ArgMatches::subcommand_name: the name of the current
// subcommand.
//
// Returns `None` if the subcommand wasn't present at runtime,
func (m *ArgMatches) SubcommandName() rs.Option[string] {
	if m.subcommand == nil {
		return rs.None[string]()
	}
	return rs.Some(m.subcommand.name)
}

// IsValidSubcommand is ArgMatches::is_valid_subcommand: check if a
// subcommand can be queried
//
// By default, `ArgMatches` functions assert on undefined `Id`s to help
// catch programmer mistakes. In some context, this doesn't work, so users
// can use this function to check before they do a query on `ArgMatches`.
func (m *ArgMatches) IsValidSubcommand(name string) bool {
	if debugAssertions {
		if name == "" {
			return true
		}
		for _, s := range m.validSubcommands {
			if s == name {
				return true
			}
		}
		return false
	}
	return true
}

// # Advanced

// ArgMatchesTryGetOne is ArgMatches::try_get_one: non-panicking version of
// ArgMatches::get_one.
func ArgMatchesTryGetOne[T any](m *ArgMatches, id string) (rs.Option[T], error) {
	value, err := argMatchesTryGetOne[T](m, id)
	if err != nil {
		return rs.None[T](), err
	}
	return value, nil
}

func argMatchesTryGetOne[T any](m *ArgMatches, id string) (rs.Option[T], ParserMatchesError) {
	arg, err := argMatchesTryGetArgT[T](m, id)
	if err != nil {
		return rs.None[T](), err
	}
	if arg == nil {
		return rs.None[T](), nil
	}
	value, ok := arg.first()
	if !ok {
		return rs.None[T](), nil
	}
	return rs.Some(unwrapDowncast[T](value)), nil // enforced by `try_get_arg_t`
}

// ArgMatchesTryGetMany is ArgMatches::try_get_many: non-panicking version
// of ArgMatches::get_many.
func ArgMatchesTryGetMany[T any](m *ArgMatches, id string) (rs.Option[[]T], error) {
	value, err := argMatchesTryGetMany[T](m, id)
	if err != nil {
		return rs.None[[]T](), err
	}
	return value, nil
}

func argMatchesTryGetMany[T any](m *ArgMatches, id string) (rs.Option[[]T], ParserMatchesError) {
	arg, err := argMatchesTryGetArgT[T](m, id)
	if err != nil {
		return rs.None[[]T](), err
	}
	if arg == nil {
		return rs.None[[]T](), nil
	}
	values := []T{}
	for _, value := range arg.valsFlatten() {
		// enforced by `try_get_arg_t`
		values = append(values, unwrapDowncast[T](value))
	}
	return rs.Some(values), nil
}

// ArgMatchesTryGetOccurrences is ArgMatches::try_get_occurrences:
// non-panicking version of ArgMatches::get_occurrences.
func ArgMatchesTryGetOccurrences[T any](m *ArgMatches, id string) (rs.Option[[][]T], error) {
	value, err := argMatchesTryGetOccurrences[T](m, id)
	if err != nil {
		return rs.None[[][]T](), err
	}
	return value, nil
}

func argMatchesTryGetOccurrences[T any](m *ArgMatches, id string) (rs.Option[[][]T], ParserMatchesError) {
	arg, err := argMatchesTryGetArgT[T](m, id)
	if err != nil {
		return rs.None[[][]T](), err
	}
	if arg == nil {
		return rs.None[[][]T](), nil
	}
	return rs.Some(downcastOccurrences[T](arg.vals)), nil
}

func downcastOccurrences[T any](vals [][]any) [][]T {
	occurrences := make([][]T, len(vals))
	for i, group := range vals {
		occurrences[i] = make([]T, len(group))
		for j, value := range group {
			occurrences[i][j] = unwrapDowncast[T](value)
		}
	}
	return occurrences
}

// TryGetRaw is ArgMatches::try_get_raw: non-panicking version of
// ArgMatches::get_raw.
func (m *ArgMatches) TryGetRaw(id string) (rs.Option[[]string], error) {
	value, err := m.tryGetRaw(id)
	if err != nil {
		return rs.None[[]string](), err
	}
	return value, nil
}

func (m *ArgMatches) tryGetRaw(id string) (rs.Option[[]string], ParserMatchesError) {
	arg, err := m.tryGetArg(id)
	if err != nil {
		return rs.None[[]string](), err
	}
	if arg == nil {
		return rs.None[[]string](), nil
	}
	values := []string{}
	values = append(values, arg.rawValsFlatten()...)
	return rs.Some(values), nil
}

// TryGetRawOccurrences is ArgMatches::try_get_raw_occurrences:
// non-panicking version of ArgMatches::get_raw_occurrences.
func (m *ArgMatches) TryGetRawOccurrences(id string) (rs.Option[[][]string], error) {
	value, err := m.tryGetRawOccurrences(id)
	if err != nil {
		return rs.None[[][]string](), err
	}
	return value, nil
}

func (m *ArgMatches) tryGetRawOccurrences(id string) (rs.Option[[][]string], ParserMatchesError) {
	arg, err := m.tryGetArg(id)
	if err != nil {
		return rs.None[[][]string](), err
	}
	if arg == nil {
		return rs.None[[][]string](), nil
	}
	occurrences := make([][]string, len(arg.rawVals))
	for i, group := range arg.rawVals {
		occurrences[i] = append([]string{}, group...)
	}
	return rs.Some(occurrences), nil
}

// ArgMatchesTryRemoveOne is ArgMatches::try_remove_one: non-panicking
// version of ArgMatches::remove_one.
func ArgMatchesTryRemoveOne[T any](m *ArgMatches, id string) (rs.Option[T], error) {
	value, err := argMatchesTryRemoveOne[T](m, id)
	if err != nil {
		return rs.None[T](), err
	}
	return value, nil
}

func argMatchesTryRemoveOne[T any](m *ArgMatches, id string) (rs.Option[T], ParserMatchesError) {
	arg, err := argMatchesTryRemoveArgT[T](m, id)
	if err != nil {
		return rs.None[T](), err
	}
	if arg == nil {
		return rs.None[T](), nil
	}
	value, ok := arg.first()
	if !ok {
		return rs.None[T](), nil
	}
	// enforced by `try_get_arg_t`
	return rs.Some(unwrapDowncast[T](value)), nil
}

// ArgMatchesTryRemoveMany is ArgMatches::try_remove_many: non-panicking
// version of ArgMatches::remove_many.
func ArgMatchesTryRemoveMany[T any](m *ArgMatches, id string) (rs.Option[[]T], error) {
	value, err := argMatchesTryRemoveMany[T](m, id)
	if err != nil {
		return rs.None[[]T](), err
	}
	return value, nil
}

func argMatchesTryRemoveMany[T any](m *ArgMatches, id string) (rs.Option[[]T], ParserMatchesError) {
	arg, err := argMatchesTryRemoveArgT[T](m, id)
	if err != nil {
		return rs.None[[]T](), err
	}
	if arg == nil {
		return rs.None[[]T](), nil
	}
	values := []T{}
	for _, value := range arg.valsFlatten() {
		// enforced by `try_get_arg_t`
		values = append(values, unwrapDowncast[T](value))
	}
	return rs.Some(values), nil
}

// ArgMatchesTryRemoveOccurrences is ArgMatches::try_remove_occurrences:
// non-panicking version of ArgMatches::remove_occurrences.
func ArgMatchesTryRemoveOccurrences[T any](m *ArgMatches, id string) (rs.Option[[][]T], error) {
	value, err := argMatchesTryRemoveOccurrences[T](m, id)
	if err != nil {
		return rs.None[[][]T](), err
	}
	return value, nil
}

func argMatchesTryRemoveOccurrences[T any](m *ArgMatches, id string) (rs.Option[[][]T], ParserMatchesError) {
	arg, err := argMatchesTryRemoveArgT[T](m, id)
	if err != nil {
		return rs.None[[][]T](), err
	}
	if arg == nil {
		return rs.None[[][]T](), nil
	}
	return rs.Some(downcastOccurrences[T](arg.vals)), nil
}

// TryContainsId is ArgMatches::try_contains_id: non-panicking version of
// ArgMatches::contains_id.
func (m *ArgMatches) TryContainsId(id string) (bool, error) {
	value, err := m.tryContainsId(id)
	if err != nil {
		return false, err
	}
	return value, nil
}

func (m *ArgMatches) tryContainsId(id string) (bool, ParserMatchesError) {
	if err := m.verifyArg(id); err != nil {
		return false, err
	}

	presence := m.args.containsKey(Id(id))
	return presence, nil
}

// TryClearId is ArgMatches::try_clear_id: clears the values for the given
// `id`
//
// Alternative to `try_remove_*` when the type is not known.
//
// Returns `Err([“MatchesError“])` if the given `id` isn't valid for
// current `ArgMatches` instance.
//
// Returns `Ok(true)` if there were any matches with the given `id`,
// `Ok(false)` otherwise.
func (m *ArgMatches) TryClearId(id string) (bool, error) {
	if err := m.verifyArg(id); err != nil {
		return false, err
	}
	_, _, removed := m.args.removeEntry(Id(id))
	return removed, nil
}

// # Private methods

func (m *ArgMatches) tryGetArg(arg string) (*parserMatchedArg, ParserMatchesError) {
	if err := m.verifyArg(arg); err != nil {
		return nil, err
	}
	if found := m.args.get(Id(arg)); found != nil {
		return *found, nil
	}
	return nil, nil
}

func argMatchesTryGetArgT[T any](m *ArgMatches, id string) (*parserMatchedArg, ParserMatchesError) {
	arg, err := m.tryGetArg(id)
	if err != nil {
		return nil, err
	}
	if arg == nil {
		return nil, nil
	}
	if err := argMatchesVerifyArgT[T](arg); err != nil {
		return nil, err
	}
	return arg, nil
}

func argMatchesTryRemoveArgT[T any](m *ArgMatches, arg string) (*parserMatchedArg, ParserMatchesError) {
	if err := m.verifyArg(arg); err != nil {
		return nil, err
	}
	id, matched, ok := m.args.removeEntry(Id(arg))
	if !ok {
		return nil, nil
	}

	expected := utilAnyValueIdOf[T]()
	actual := matched.inferTypeId(expected)
	if actual.equal(expected) {
		return matched, nil
	}
	m.args.insert(id, matched)
	return nil, &ParserMatchesError_Downcast{Actual: actual, Expected: expected}
}

func argMatchesVerifyArgT[T any](arg *parserMatchedArg) ParserMatchesError {
	expected := utilAnyValueIdOf[T]()
	actual := arg.inferTypeId(expected)
	if expected.equal(actual) {
		return nil
	}
	return &ParserMatchesError_Downcast{Actual: actual, Expected: expected}
}

func (m *ArgMatches) verifyArg(arg string) ParserMatchesError {
	if debugAssertions {
		if arg == idExternal || idsContain(m.validArgs, Id(arg)) {
		} else {
			return &ParserMatchesError_UnknownArgument{}
		}
	}
	return nil
}

func (m *ArgMatches) getArg(arg string) *parserMatchedArg {
	if debugAssertions {
		if arg == idExternal || idsContain(m.validArgs, Id(arg)) {
		} else {
			rs.PanicAt(2, "`"+strDebug(arg)+"` is not an id of an argument or a group.\n"+
				"Make sure you're using the name of the argument itself "+
				"and not the name of short or long flags.")
		}
	}

	if found := m.args.get(Id(arg)); found != nil {
		return *found
	}
	return nil
}

func (m *ArgMatches) getSubcommand(name string) *parserSubCommand {
	if debugAssertions {
		valid := name == ""
		for _, s := range m.validSubcommands {
			if s == name {
				valid = true
				break
			}
		}
		if !valid {
			rs.PanicAt(2, "`"+name+"` is not a name of a subcommand.")
		}
	}

	if sc := m.subcommand; sc != nil {
		if sc.name == name {
			return sc
		}
	}

	return nil
}

// Clone is the derived Clone of ArgMatches.
func (m *ArgMatches) Clone() *ArgMatches {
	cloned := &ArgMatches{
		validArgs:        cloneIds(m.validArgs),
		validSubcommands: append([]string(nil), m.validSubcommands...),
	}
	cloned.args.keys = cloneIds(m.args.keys)
	cloned.args.values = make([]*parserMatchedArg, len(m.args.values))
	for i, value := range m.args.values {
		cloned.args.values[i] = value.clone()
	}
	if m.subcommand != nil {
		cloned.subcommand = &parserSubCommand{name: m.subcommand.name, matches: m.subcommand.matches.Clone()}
	}
	return cloned
}

// Equal is the derived PartialEq of ArgMatches.
func (m *ArgMatches) Equal(other *ArgMatches) bool {
	if len(m.validArgs) != len(other.validArgs) || len(m.validSubcommands) != len(other.validSubcommands) {
		return false
	}
	for i := range m.validArgs {
		if m.validArgs[i] != other.validArgs[i] {
			return false
		}
	}
	for i := range m.validSubcommands {
		if m.validSubcommands[i] != other.validSubcommands[i] {
			return false
		}
	}
	if len(m.args.keys) != len(other.args.keys) {
		return false
	}
	for i := range m.args.keys {
		if m.args.keys[i] != other.args.keys[i] || !m.args.values[i].equal(other.args.values[i]) {
			return false
		}
	}
	if (m.subcommand == nil) != (other.subcommand == nil) {
		return false
	}
	if m.subcommand != nil {
		return m.subcommand.name == other.subcommand.name && m.subcommand.matches.Equal(other.subcommand.matches)
	}
	return true
}

// parserSubCommand is SubCommand.
type parserSubCommand struct {
	name    string
	matches *ArgMatches
}

// unwrapDowncast is unwrap_downcast_ref and unwrap_downcast_into: the
// stored value as a T, which the type check before it guarantees.
func unwrapDowncast[T any](value any) T {
	typed, ok := value.(T)
	if !ok {
		rs.PanicAt(1, internalErrorMsg)
	}
	return typed
}

// rustTypeName names a Go type the way Rust names the type it stands for,
// for the primitives; other types keep their Go name.
func rustTypeName(t reflect.Type) string {
	if t.PkgPath() == "" {
		switch t.Kind() {
		case reflect.Bool:
			return "bool"
		case reflect.String:
			return "alloc::string::String"
		case reflect.Int8:
			return "i8"
		case reflect.Int16:
			return "i16"
		case reflect.Int32:
			return "i32"
		case reflect.Int64:
			return "i64"
		case reflect.Int:
			return "isize"
		case reflect.Uint8:
			return "u8"
		case reflect.Uint16:
			return "u16"
		case reflect.Uint32:
			return "u32"
		case reflect.Uint64:
			return "u64"
		case reflect.Uint:
			return "usize"
		case reflect.Float32:
			return "f32"
		case reflect.Float64:
			return "f64"
		}
	}
	return t.String()
}
