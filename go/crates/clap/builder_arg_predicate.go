// Port of clap_builder 4.6.0 src/builder/arg_predicate.rs

package clap

import "github.com/BrokkAi/mjolnir/go/rs"

// BuilderArgPredicate is clap::builder::ArgPredicate: operations to perform
// on argument values.
//
// These do not apply to ValueSource::DefaultValue.
type BuilderArgPredicate interface {
	isBuilderArgPredicate()
}

// BuilderArgPredicate_IsPresent is ArgPredicate::IsPresent: is the argument
// present?
type BuilderArgPredicate_IsPresent struct{}

// BuilderArgPredicate_Equals is ArgPredicate::Equals: does the argument
// match the specified value?
type BuilderArgPredicate_Equals struct{ F0 string }

func (*BuilderArgPredicate_IsPresent) isBuilderArgPredicate() {}
func (*BuilderArgPredicate_Equals) isBuilderArgPredicate()    {}

var _ = rs.RegisterEnum[BuilderArgPredicate]("ArgPredicate",
	rs.Variant[BuilderArgPredicate_IsPresent]("IsPresent", rs.ShapeUnit),
	rs.Variant[BuilderArgPredicate_Equals]("Equals", rs.ShapeTuple),
)

// BuilderArgPredicateFromStr is `impl<S: Into<OsStr>> From<S> for
// ArgPredicate`: the argument equals other.
func BuilderArgPredicateFromStr(other string) BuilderArgPredicate {
	return &BuilderArgPredicate_Equals{F0: other}
}

var argPredicateIsPresent BuilderArgPredicate = &BuilderArgPredicate_IsPresent{}
