// Port of clap_builder 4.6.0 src/builder/styling.rs

package clap

import "github.com/BrokkAi/mjolnir/go/rs"

// BuilderStyles is clap::builder::Styles: terminal styling definitions.
//
// See also Command::styles.
//
// A Styles is a value: every setter returns a changed copy.
type BuilderStyles struct {
	header       BuilderStylingStyle
	error_       BuilderStylingStyle
	usage        BuilderStylingStyle
	literal      BuilderStylingStyle
	placeholder  BuilderStylingStyle
	valid        BuilderStylingStyle
	invalid      BuilderStylingStyle
	context      BuilderStylingStyle
	contextValue rs.Option[BuilderStylingStyle]
}

// BuilderStylesPlain is Styles::plain: no terminal styling.
func BuilderStylesPlain() BuilderStyles {
	return BuilderStyles{}
}

// BuilderStylesStyled is Styles::styled: default terminal styling.
func BuilderStylesStyled() BuilderStyles {
	return BuilderStyles{
		header: BuilderStylingStyleNew().Bold().Underline(),
		error_: BuilderStylingStyleNew().
			FgColor(rs.Some[BuilderStylingColor](&BuilderStylingColor_Ansi{F0: BuilderStylingAnsiColor_Red})).
			Bold(),
		usage:        BuilderStylingStyleNew().Bold().Underline(),
		literal:      BuilderStylingStyleNew().Bold(),
		placeholder:  BuilderStylingStyleNew(),
		valid:        BuilderStylingStyleNew().FgColor(rs.Some[BuilderStylingColor](&BuilderStylingColor_Ansi{F0: BuilderStylingAnsiColor_Green})),
		invalid:      BuilderStylingStyleNew().FgColor(rs.Some[BuilderStylingColor](&BuilderStylingColor_Ansi{F0: BuilderStylingAnsiColor_Yellow})),
		context:      BuilderStylingStyleNew(),
		contextValue: rs.None[BuilderStylingStyle](),
	}
}

// BuilderStylesDefault is `impl Default for Styles`.
func BuilderStylesDefault() BuilderStyles {
	return BuilderStylesStyled()
}

// Header is Styles::header: general heading style, e.g. `Commands`.
func (s BuilderStyles) Header(style BuilderStylingStyle) BuilderStyles {
	s.header = style
	return s
}

// Error is Styles::error: error heading.
func (s BuilderStyles) Error(style BuilderStylingStyle) BuilderStyles {
	s.error_ = style
	return s
}

// Usage is Styles::usage: usage heading.
func (s BuilderStyles) Usage(style BuilderStylingStyle) BuilderStyles {
	s.usage = style
	return s
}

// Literal is Styles::literal: literal command-line syntax, e.g. `--help`.
func (s BuilderStyles) Literal(style BuilderStylingStyle) BuilderStyles {
	s.literal = style
	return s
}

// Placeholder is Styles::placeholder: descriptions within command-line
// syntax, e.g. `value_name`.
func (s BuilderStyles) Placeholder(style BuilderStylingStyle) BuilderStyles {
	s.placeholder = style
	return s
}

// Valid is Styles::valid: highlight suggested usage.
func (s BuilderStyles) Valid(style BuilderStylingStyle) BuilderStyles {
	s.valid = style
	return s
}

// Invalid is Styles::invalid: highlight invalid usage.
func (s BuilderStyles) Invalid(style BuilderStylingStyle) BuilderStyles {
	s.invalid = style
	return s
}

// Context is Styles::context: highlight all specified contexts, e.g.
// `[default: false]`.
//
// To specialize the style of the value within the context, see
// ContextValue.
func (s BuilderStyles) Context(style BuilderStylingStyle) BuilderStyles {
	s.context = style
	return s
}

// ContextValue is Styles::context_value: highlight values within all of the
// context, e.g. the `false` in `[default: false]`.
//
// If not explicitly set, falls back to `context`'s style.
func (s BuilderStyles) ContextValue(style BuilderStylingStyle) BuilderStyles {
	s.contextValue = rs.Some(style)
	return s
}

// GetHeader is Styles::get_header: general heading style, e.g. `Commands`.
func (s *BuilderStyles) GetHeader() BuilderStylingStyle { return s.header }

// GetError is Styles::get_error: error heading.
func (s *BuilderStyles) GetError() BuilderStylingStyle { return s.error_ }

// GetUsage is Styles::get_usage: usage heading.
func (s *BuilderStyles) GetUsage() BuilderStylingStyle { return s.usage }

// GetLiteral is Styles::get_literal: literal command-line syntax, e.g.
// `--help`.
func (s *BuilderStyles) GetLiteral() BuilderStylingStyle { return s.literal }

// GetPlaceholder is Styles::get_placeholder: descriptions within
// command-line syntax, e.g. `value_name`.
func (s *BuilderStyles) GetPlaceholder() BuilderStylingStyle { return s.placeholder }

// GetValid is Styles::get_valid: highlight suggested usage.
func (s *BuilderStyles) GetValid() BuilderStylingStyle { return s.valid }

// GetInvalid is Styles::get_invalid: highlight invalid usage.
func (s *BuilderStyles) GetInvalid() BuilderStylingStyle { return s.invalid }

// GetContext is Styles::get_context: highlight all specified contexts, e.g.
// `[default: false]`.
//
// To specialize the style of the value within the context, see
// GetContextValue.
func (s *BuilderStyles) GetContext() BuilderStylingStyle { return s.context }

// GetContextValue is Styles::get_context_value: highlight values within all
// of the context, e.g. the `false` in `[default: false]`.
//
// If not explicitly set, falls back to `context`'s style.
func (s *BuilderStyles) GetContextValue() BuilderStylingStyle {
	if style, ok := s.contextValue.Get(); ok {
		return style
	}
	return s.context
}

// Clone is `impl Clone for Styles`.
func (s *BuilderStyles) Clone() BuilderStyles { return *s }

// stylesDefault is `impl Default for &Styles`: the styles a command has when
// none were set.
var stylesDefault = BuilderStylesStyled()
