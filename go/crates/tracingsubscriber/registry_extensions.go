// Port of tracing-subscriber 0.3.23 src/registry/extensions.rs

package tracingsubscriber

import (
	"reflect"
	"sync"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// The extensions of a span are a map from a type to one value of that type,
// which layers use to keep their own data with the span. Rust's methods are
// generic, so here they are functions: RegistryExtensionsGet[T](ext) is
// `ext.get::<T>()`. A value is stored as a *T, so what get returns can be
// changed in place through the mutable guard.

// RegistryExtensions is tracing_subscriber::registry::Extensions: the
// extensions of a span, read-locked until Drop.
type RegistryExtensions struct {
	inner   *extensionsInner
	dropped bool
}

// RegistryExtensionsMut is tracing_subscriber::registry::ExtensionsMut: the
// extensions of a span, write-locked until Drop.
type RegistryExtensionsMut struct {
	inner   *extensionsInner
	dropped bool
}

// extensionsInner is ExtensionsInner behind its RwLock.
type extensionsInner struct {
	lock sync.RWMutex
	m    map[reflect.Type]any
}

func (e *extensionsInner) read() *RegistryExtensions {
	e.lock.RLock()
	return &RegistryExtensions{inner: e}
}

func (e *extensionsInner) write() *RegistryExtensionsMut {
	e.lock.Lock()
	return &RegistryExtensionsMut{inner: e}
}

// Drop releases the read lock.
func (e *RegistryExtensions) Drop() {
	if e.dropped {
		return
	}
	e.dropped = true
	e.inner.lock.RUnlock()
}

// Drop releases the write lock.
func (e *RegistryExtensionsMut) Drop() {
	if e.dropped {
		return
	}
	e.dropped = true
	e.inner.lock.Unlock()
}

// RegistryExtensionsGet is Extensions::get.
func RegistryExtensionsGet[T any](e *RegistryExtensions) rs.Option[*T] {
	return extensionsGet[T](e.inner)
}

// RegistryExtensionsMutGetMut is ExtensionsMut::get_mut.
func RegistryExtensionsMutGetMut[T any](e *RegistryExtensionsMut) rs.Option[*T] {
	return extensionsGet[T](e.inner)
}

// RegistryExtensionsMutInsert is ExtensionsMut::insert: it panics when the
// span already has a value of this type.
func RegistryExtensionsMutInsert[T any](e *RegistryExtensionsMut, value T) {
	rs.Assert(RegistryExtensionsMutReplace(e, value).IsNone(), "assertion failed: self.replace(val).is_none()")
}

// RegistryExtensionsMutReplace is ExtensionsMut::replace: it stores value and
// returns the value of this type the span had.
func RegistryExtensionsMutReplace[T any](e *RegistryExtensionsMut, value T) rs.Option[T] {
	if e.inner.m == nil {
		e.inner.m = make(map[reflect.Type]any)
	}
	key := reflect.TypeFor[T]()
	previous, ok := e.inner.m[key]
	e.inner.m[key] = &value
	if !ok {
		return rs.None[T]()
	}
	return rs.Some(*previous.(*T))
}

// RegistryExtensionsMutRemove is ExtensionsMut::remove.
func RegistryExtensionsMutRemove[T any](e *RegistryExtensionsMut) rs.Option[T] {
	key := reflect.TypeFor[T]()
	previous, ok := e.inner.m[key]
	if !ok {
		return rs.None[T]()
	}
	delete(e.inner.m, key)
	return rs.Some(*previous.(*T))
}

func extensionsGet[T any](inner *extensionsInner) rs.Option[*T] {
	value, ok := inner.m[reflect.TypeFor[T]()]
	if !ok {
		return rs.None[*T]()
	}
	return rs.Some(value.(*T))
}

// clear is ExtensionsInner::clear.
func (e *extensionsInner) clear() {
	e.lock.Lock()
	defer e.lock.Unlock()
	clear(e.m)
}
