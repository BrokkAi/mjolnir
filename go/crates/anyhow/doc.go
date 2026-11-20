// Package anyhow stands in for the Rust crate anyhow 1.0.102.
//
// In the port an anyhow::Error is a Go error. The functions here build such
// errors and read them the way the crate does, so that the text a user or a
// test sees is the same in both implementations:
//
//	anyhow!("..") / bail!("..")          anyhow.Errorf("..", args...)
//	anyhow!(message) / Error::msg(m)      anyhow.Msg(message)
//	anyhow!(error) / Error::new / From    anyhow.New(err)
//	result.context(c)?                    anyhow.Context(err, c)
//	result.with_context(|| ..)?           anyhow.WithContext(err, func() string { .. })
//	option.context(c)?                    anyhow.OptionContext(option, c)
//	error.chain()                         anyhow.Chain(err)
//	error.root_cause()                    anyhow.RootCause(err)
//	error.downcast_ref::<T>()             anyhow.DowncastRef[T](err)
//	error.is::<T>()                       anyhow.Is[T](err)
//	error.downcast::<T>()                 anyhow.Downcast[T](err)
//	ensure!(a == b)                       anyhow.EnsureFailedDebug("a == b", a, b)
//
// # The text forms
//
// An anyhow error prints in four ways, and they differ on purpose:
//
//	{}     only the outermost message                      DisplayString, Error()
//	{:#}   the whole chain joined by ": "                  AlternateString
//	{:?}   the outermost message, then "Caused by:"        DebugString
//	{:#?}  the struct form, Error { context, source }      PrettyDebugString
//
// The first three are built from the plain message of each error in the
// chain, so they need nothing from the formatter. rs.Format reaches the same
// code through FmtDisplay and FmtDebug.
//
// Never wrap with fmt.Errorf("...: %w", err). That collapses the forms into
// one string: the "{}" form then already holds the whole chain, "{:#}" repeats
// it, and Chain, RootCause and DowncastRef see one opaque error.
//
// # What an error is made of
//
// As in the crate, an error made here is a handle (anyhow::Error) on an error
// object (the `dyn std::error::Error` inside it). The functions of this
// package treat a handle as the crate treats an anyhow::Error: DowncastRef
// looks at the outermost object and through every context layer, and {:?}
// prints the "Caused by:" form. Chain and RootCause return the objects, as
// the crate's chain() returns `&dyn Error`: on one of those DowncastRef only
// sees that element's own type, and {:?} prints the element's own Debug form.
//
// Any Go error may be the root of a chain, wrapped or not. One that reports a
// source through `Unwrap() error`, which is how every ported
// `impl std::error::Error` spells `source()`, contributes its sources to the
// chain. The errors of this package follow the same convention: Unwrap is the
// next element of Chain. errors.Is and errors.As work through context layers
// and find a typed context where DowncastRef finds it.
//
// An error that was never passed to New prints with whatever {:#} and {:?}
// its own type has. When Rust held it in an anyhow::Error, wrap it with New at
// the point the Rust code converted it (`?` into anyhow::Result, `.into()`,
// Error::new, anyhow!(err)), so that {:#} prints its sources and {:?} prints
// its message rather than its own Debug form. The wrapping changes nothing
// else.
//
// # Covered
//
// error.rs (new, msg, from_boxed, context, chain, root_cause, is, downcast,
// downcast_ref, Deref/AsRef to the error object), context.rs (Context for
// Result and for Option, with a context of any Display type), chain.rs,
// fmt.rs, wrapper.rs, kind.rs and macros.rs (anyhow!, bail!, ensure! with a
// message), ensure.rs (the message of ensure! without one).
//
// # Not covered
//
//   - Backtraces. The crate captures one only when RUST_BACKTRACE or
//     RUST_LIB_BACKTRACE asks for it, and Mjolnir runs with neither; with both
//     unset std::backtrace::Backtrace::capture is disabled and {:?} has no
//     "Stack backtrace:" section, which is what this package prints. Setting
//     the variables changes the Rust output and not this one.
//   - downcast_mut. DowncastRef with a pointer type gives the same access.
//   - Dropping. Go errors are collected, so a value held by an error is not
//     dropped at a known point; the crate's drop tests have no counterpart.
//   - The parsing ensure! does of its condition at compile time. The caller
//     passes the condition's text, and says whether it was a comparison of
//     two Debug values (EnsureFailedDebug) or not (EnsureFailed).
//   - into_boxed_dyn_error and reallocate_into_boxed_dyn_error_without_backtrace.
//     A Go error is already the boxed form.
//
// # Where Go differs
//
//   - `&'static str` and `String` are both string, so DowncastRef[string]
//     finds a message made from a literal and one made by formatting alike.
//   - DowncastRef[T] matches a stored T or *T for either spelling of the type
//     argument, because one Rust type is held by value or by pointer in Go
//     depending on how the type was ported.
//   - A context value of a type whose Display writes in several pieces is
//     escaped as one piece by {:#?}. The crate escapes each piece on its own,
//     which shows only when a later piece starts with a combining character.
package anyhow
