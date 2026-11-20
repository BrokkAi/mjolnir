// Port of tracing-subscriber 0.3.23 src/util.rs

package tracingsubscriber

import (
	"errors"

	"github.com/BrokkAi/mjolnir/go/crates/tracing"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// The SubscriberInitExt trait, implemented for everything that becomes a
// Dispatch: a subscriber (a new Dispatch is made of it) or a *Dispatch.
//
// tracing-subscriber's tracing-log feature also installs a `log` crate
// logger in try_init and set_default, which forwards records of crates that
// log with `log`. The port has no `log` crate, so nothing is installed.

// SubscriberInitExtSetDefault is SubscriberInitExt::set_default.
func SubscriberInitExtSetDefault(subscriber any) *tracing.DispatcherDefaultGuard {
	return tracing.DispatcherSetDefault(intoDispatch(subscriber))
}

// SubscriberInitExtTryInit is SubscriberInitExt::try_init: the global
// default, or a UtilTryInitError when one was already set.
func SubscriberInitExtTryInit(subscriber any) error {
	if err := tracing.DispatcherSetGlobalDefault(intoDispatch(subscriber)); err != nil {
		return utilTryInitErrorNew(err)
	}
	return nil
}

// SubscriberInitExtInit is SubscriberInitExt::init.
func SubscriberInitExtInit(subscriber any) {
	if err := SubscriberInitExtTryInit(subscriber); err != nil {
		rs.PanicAt(1, "failed to set global default subscriber: "+rs.DebugString(err))
	}
}

func intoDispatch(subscriber any) *tracing.Dispatch {
	switch subscriber := subscriber.(type) {
	case *tracing.Dispatch:
		return subscriber
	case tracing.Subscriber:
		return tracing.DispatchNew(subscriber)
	}
	panic("tracing_subscriber: a subscriber must be a *tracing.Dispatch or implement tracing.Subscriber")
}

// UtilTryInitError is tracing_subscriber::util::TryInitError: the global
// default could not be set.
type UtilTryInitError struct {
	inner error
}

func utilTryInitErrorNew(e error) *UtilTryInitError {
	return &UtilTryInitError{inner: e}
}

// Error is `impl Display for TryInitError`: the inner error's text.
func (e *UtilTryInitError) Error() string { return e.inner.Error() }

func (e *UtilTryInitError) String() string { return e.Error() }

// FmtDisplay is `impl Display for TryInitError`.
func (e *UtilTryInitError) FmtDisplay(f *rs.Formatter) error { return rs.DisplayFmt(e.inner, f) }

// FmtDebug is `impl Debug for TryInitError`: the inner error's Debug form.
func (e *UtilTryInitError) FmtDebug(f *rs.Formatter) error { return rs.DebugFmt(e.inner, f) }

// Unwrap is TryInitError::source: the inner error's source, not the inner
// error.
func (e *UtilTryInitError) Unwrap() error { return errors.Unwrap(e.inner) }
