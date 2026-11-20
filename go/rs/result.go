// Port of core 1.99.1 src/result.rs

package rs

// Result is Rust's Result<T, E> for the places where a result is a value: a
// channel element, a struct field, a collection element. A function that
// returns Result<T, E> returns (T, error) instead.
type Result[T any] struct {
	Value T
	Err   error
}

// Ok returns Ok(value).
func Ok[T any](value T) Result[T] { return Result[T]{Value: value} }

// Err returns Err(err).
func Err[T any](err error) Result[T] { return Result[T]{Err: err} }

// ResultOf packs a function's (T, error) into a Result.
func ResultOf[T any](value T, err error) Result[T] {
	if err != nil {
		var zero T
		return Result[T]{Value: zero, Err: err}
	}
	return Result[T]{Value: value}
}

// IsOk reports whether the result is Ok.
func (r Result[T]) IsOk() bool { return r.Err == nil }

// IsErr reports whether the result is Err.
func (r Result[T]) IsErr() bool { return r.Err != nil }

// Get unpacks the result into Go's (T, error) form, which is what `?` and
// `match` on a result work on.
func (r Result[T]) Get() (T, error) { return r.Value, r.Err }

// Ok is Result::ok.
func (r Result[T]) Ok() Option[T] {
	if r.Err != nil {
		return Option[T]{}
	}
	return Some(r.Value)
}

// Unwrap is Result::unwrap.
func (r Result[T]) Unwrap() T {
	if r.Err != nil {
		PanicAt(1, "called `Result::unwrap()` on an `Err` value: "+DebugString(r.Err))
	}
	return r.Value
}

// Expect is Result::expect.
func (r Result[T]) Expect(message string) T {
	if r.Err != nil {
		PanicAt(1, message+": "+DebugString(r.Err))
	}
	return r.Value
}

// Unwrap is `result.unwrap()` on a (T, error) pair: it returns value, or
// panics with Rust's message when err is not nil.
//
//	config := rs.Unwrap(loadConfig(path))
func Unwrap[T any](value T, err error) T {
	if err != nil {
		PanicAt(1, "called `Result::unwrap()` on an `Err` value: "+DebugString(err))
	}
	return value
}

// Expect is `result.expect(message)` on a (T, error) pair. Go cannot spread a
// call's two results in front of another argument, so the pair is packed
// first:
//
//	config := rs.Expect(rs.ResultOf(loadConfig(path)), "config loads")
func Expect[T any](result Result[T], message string) T {
	if result.Err != nil {
		PanicAt(1, message+": "+DebugString(result.Err))
	}
	return result.Value
}

// UnwrapUnit is `result.unwrap()` for a Result<(), E>, which Go holds as a
// bare error.
func UnwrapUnit(err error) {
	if err != nil {
		PanicAt(1, "called `Result::unwrap()` on an `Err` value: "+DebugString(err))
	}
}

// ExpectUnit is `result.expect(message)` for a Result<(), E>.
func ExpectUnit(err error, message string) {
	if err != nil {
		PanicAt(1, message+": "+DebugString(err))
	}
}

// UnwrapErr is Result::unwrap_err on a (T, error) pair.
func UnwrapErr[T any](value T, err error) error {
	if err == nil {
		PanicAt(1, "called `Result::unwrap_err()` on an `Ok` value: "+DebugString(value))
	}
	return err
}

// ExpectErr is Result::expect_err.
func ExpectErr[T any](result Result[T], message string) error {
	if result.Err == nil {
		PanicAt(1, message+": "+DebugString(result.Value))
	}
	return result.Err
}

// UnwrapOr is Result::unwrap_or on a (T, error) pair.
func UnwrapOr[T any](value T, err error, fallback T) T {
	if err != nil {
		return fallback
	}
	return value
}

// UnwrapOrElse is Result::unwrap_or_else on a (T, error) pair.
func UnwrapOrElse[T any](value T, err error, f func(error) T) T {
	if err != nil {
		return f(err)
	}
	return value
}

// OkOption is Result::ok on a (T, error) pair: the value, or None on error.
func OkOption[T any](value T, err error) Option[T] {
	if err != nil {
		return Option[T]{}
	}
	return Some(value)
}

// ErrOption is Result::err on a (T, error) pair.
func ErrOption[T any](_ T, err error) Option[error] {
	if err == nil {
		return Option[error]{}
	}
	return Some(err)
}
