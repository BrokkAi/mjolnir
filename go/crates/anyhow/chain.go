// Port of anyhow 1.0.102 src/chain.rs

package anyhow

// The crate's Chain is an iterator that follows `source()` from a head
// error. Here Chain (error.go) returns the whole walk as a slice, which is
// also its len(), its rev() and its last(). What remains of this file is the
// step of the walk: std::error::Error::source for a Go error.

// unwrapper is how a ported `impl std::error::Error for T` reports its
// `source()`: the method `Unwrap() error`, nil for None.
type unwrapper interface {
	Unwrap() error
}

// source is `error.source()`: the next error in the chain, or nil.
//
// The error objects of this package answer as the crate's types do, and any
// other error answers through Unwrap. An anyhow error returned as a source
// stands for its error object: Rust code can only put `error.as_ref()` there,
// an anyhow::Error not being a `dyn Error` itself.
func source(err error) error {
	var next error
	switch object := err.(type) {
	case *errorImpl:
		// ErrorImpl<E>::source, reached through a boxed anyhow error.
		return source(object.object)
	case *contextError:
		next = object.error
	case *messageError, *displayError:
		return nil
	case *boxedError:
		// BoxedError::source is `self.0.source()`.
		return source(object.inner)
	case unwrapper:
		next = object.Unwrap()
	}
	return errorObject(next)
}
