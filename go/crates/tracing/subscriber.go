// Port of tracing 0.1.44 src/subscriber.rs

package tracing

// SubscriberWithDefault is tracing::subscriber::with_default: f run with
// subscriber, in a new Dispatch, as the default of the calling thread.
func SubscriberWithDefault[T any](subscriber Subscriber, f func() T) T {
	return DispatcherWithDefault(DispatchNew(subscriber), f)
}

// SubscriberSetGlobalDefault is tracing::subscriber::set_global_default: it
// makes subscriber, in a new Dispatch, the default of every thread that has
// none of its own, for the rest of the program.
func SubscriberSetGlobalDefault(subscriber Subscriber) error {
	return DispatcherSetGlobalDefault(DispatchNew(subscriber))
}

// SubscriberSetDefault is tracing::subscriber::set_default: it makes
// subscriber, in a new Dispatch, the default of the calling thread until the
// guard is dropped.
//
//	guard := tracing.SubscriberSetDefault(rs.Clone(log))
//	defer guard.Drop()
func SubscriberSetDefault(subscriber Subscriber) *DispatcherDefaultGuard {
	return DispatcherSetDefault(DispatchNew(subscriber))
}
