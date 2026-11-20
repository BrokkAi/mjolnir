// Port of tracing-subscriber 0.3.23 src/filter/env/builder.rs

package tracingsubscriber

import (
	"io"
	"os"
	"strings"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/crates/tracing"
	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rsio"
	"github.com/BrokkAi/mjolnir/go/rs/rspath"
)

// FilterBuilder is tracing_subscriber::filter::Builder: how an EnvFilter is
// made, EnvFilterBuilder() being EnvFilter::builder().
type FilterBuilder struct {
	regex            bool
	env              rs.Option[string]
	defaultDirective rs.Option[*FilterDirective]
}

// WithRegex is Builder::with_regex: whether a field value in a directive is
// a regular expression. With false it is the Debug text the value must
// start.
func (b FilterBuilder) WithRegex(regex bool) FilterBuilder {
	b.regex = regex
	return b
}

// WithDefaultDirective is Builder::with_default_directive: the directive a
// filter gets when the text holds none.
func (b FilterBuilder) WithDefaultDirective(defaultDirective *FilterDirective) FilterBuilder {
	b.defaultDirective = rs.Some(defaultDirective)
	return b
}

// WithEnvVar is Builder::with_env_var: the environment variable the from_env
// methods read, RUST_LOG by default.
func (b FilterBuilder) WithEnvVar(variable string) FilterBuilder {
	b.env = rs.Some(variable)
	return b
}

// ParseLossy is Builder::parse_lossy: a filter of the directives that parse.
// Each one that does not is reported on standard error as
//
//	ignoring `<directive>`: <error>
//
// and left out.
func (b FilterBuilder) ParseLossy(dirs string) *EnvFilter {
	var directives []*FilterDirective
	for _, s := range strings.Split(dirs, ",") {
		if s == "" {
			continue
		}
		d, err := directiveParse(s, b.regex)
		if err != nil {
			eprintln("ignoring `" + s + "`: " + err.Error())
			continue
		}
		directives = append(directives, d)
	}
	return b.fromDirectives(directives)
}

// Parse is Builder::parse: a filter of the directives, or the error of the
// first that does not parse.
func (b FilterBuilder) Parse(dirs string) (*EnvFilter, error) {
	if dirs == "" {
		return b.fromDirectives(nil), nil
	}
	var directives []*FilterDirective
	for _, s := range strings.Split(dirs, ",") {
		if s == "" {
			continue
		}
		d, err := directiveParse(s, b.regex)
		if err != nil {
			return nil, err
		}
		directives = append(directives, d)
	}
	return b.fromDirectives(directives), nil
}

// FromEnvLossy is Builder::from_env_lossy: ParseLossy of the environment
// variable, empty when it is unset or not Unicode.
func (b FilterBuilder) FromEnvLossy() *EnvFilter {
	variable, _ := envVar(b.envVarName())
	return b.ParseLossy(variable)
}

// FromEnv is Builder::from_env: Parse of the environment variable, empty
// when it is unset or not Unicode.
func (b FilterBuilder) FromEnv() (*EnvFilter, error) {
	variable, _ := envVar(b.envVarName())
	filter, err := b.Parse(variable)
	if err != nil {
		return nil, filterFromEnvErrorFromParse(err.(*FilterParseError))
	}
	return filter, nil
}

// TryFromEnv is Builder::try_from_env: Parse of the environment variable,
// which must be set and Unicode.
func (b FilterBuilder) TryFromEnv() (*EnvFilter, error) {
	variable, varErr := envVar(b.envVarName())
	if varErr != nil {
		return nil, filterFromEnvErrorFromVar(varErr)
	}
	filter, err := b.Parse(variable)
	if err != nil {
		return nil, filterFromEnvErrorFromParse(err.(*FilterParseError))
	}
	return filter, nil
}

// fromDirectives is Builder::from_directives.
//
// The crate first warns about each directive that enables a level above
// tracing's STATIC_MAX_LEVEL. Mjolnir enables none of tracing's max_level
// features, so that level is TRACE, no directive is above it, and the
// warning is never printed; it is not ported.
func (b FilterBuilder) fromDirectives(directives []*FilterDirective) *EnvFilter {
	for _, directive := range directives {
		if !b.regex {
			directive.deregexify()
		}
	}

	dynamics, statics := directiveMakeTables(directives)
	hasDynamics := !dynamics.isEmpty()

	filter := &EnvFilter{
		statics:     statics,
		dynamics:    dynamics,
		hasDynamics: hasDynamics,
		byId:        make(map[tracing.SpanId]*spanMatcher),
		byCs:        make(map[tracing.CallsiteIdentifier]*callsiteMatcher),
		regex:       b.regex,
	}

	if !hasDynamics && filter.statics.isEmpty() {
		if defaultDirective, ok := b.defaultDirective.Get(); ok {
			filter = filter.AddDirective(defaultDirective.clone())
		}
	}

	return filter
}

func (b FilterBuilder) envVarName() string {
	return b.env.UnwrapOr(EnvFilterDEFAULT_ENV)
}

// FilterBuilderDefault is `impl Default for Builder`.
func FilterBuilderDefault() FilterBuilder {
	return FilterBuilder{
		regex:            true,
		env:              rs.None[string](),
		defaultDirective: rs.None[*FilterDirective](),
	}
}

// diagnostics is where `eprintln!` writes: standard error.
var diagnostics io.Writer = rsio.Stderr()

// eprintln is `eprintln!("{}", line)`. Like Rust's it panics when standard
// error cannot be written.
func eprintln(line string) {
	if err := rsio.WriteAll(diagnostics, []byte(line+"\n")); err != nil {
		rs.PanicAt(1, "failed printing to stderr: "+err.Error())
	}
}

// envVarError is std::env::VarError.
type envVarError struct {
	// notUnicode holds the value for VarError::NotUnicode; absent is
	// VarError::NotPresent.
	notUnicode rs.Option[string]
}

func (e *envVarError) Error() string {
	if value, ok := e.notUnicode.Get(); ok {
		return "environment variable was not valid unicode: " + rspath.DebugString(value)
	}
	return "environment variable not found"
}

// envVar is std::env::var.
func envVar(name string) (string, *envVarError) {
	value, ok := os.LookupEnv(name)
	if !ok {
		return "", &envVarError{notUnicode: rs.None[string]()}
	}
	if !utf8.ValidString(value) {
		return "", &envVarError{notUnicode: rs.Some(value)}
	}
	return value, nil
}
