// Port of core 1.99.1 src/option.rs, src/result.rs (derive(Debug)) and
// src/fmt/mod.rs (the Debug impls of `()` and of tuples)

package rs

// FmtDebug is the Debug that Option derives: `Some(value)` or `None`.
func (o Option[T]) FmtDebug(f *Formatter) error {
	if o.some {
		return f.DebugTuple("Some").Field(o.value).Finish()
	}
	return f.WriteStr("None")
}

// FmtDebug is the Debug that Result derives: `Ok(value)` or `Err(error)`.
func (r Result[T]) FmtDebug(f *Formatter) error {
	if r.Err != nil {
		return f.DebugTuple("Err").Field(r.Err).Finish()
	}
	return f.DebugTuple("Ok").Field(r.Value).Finish()
}

// FmtDebug is `impl Debug for ()`.
func (Unit) FmtDebug(f *Formatter) error {
	return f.Pad("()")
}

// FmtDebug is `impl Debug for (A, B)`.
func (t Tuple2[A, B]) FmtDebug(f *Formatter) error {
	return f.DebugTuple("").Field(t.F0).Field(t.F1).Finish()
}

// FmtDebug is `impl Debug for (A, B, C)`.
func (t Tuple3[A, B, C]) FmtDebug(f *Formatter) error {
	return f.DebugTuple("").Field(t.F0).Field(t.F1).Field(t.F2).Finish()
}

// FmtDebug is `impl Debug for (A, B, C, D)`.
func (t Tuple4[A, B, C, D]) FmtDebug(f *Formatter) error {
	return f.DebugTuple("").Field(t.F0).Field(t.F1).Field(t.F2).Field(t.F3).Finish()
}

// FmtDebug is `impl Debug for (A, B, C, D, E)`.
func (t Tuple5[A, B, C, D, E]) FmtDebug(f *Formatter) error {
	return f.DebugTuple("").Field(t.F0).Field(t.F1).Field(t.F2).Field(t.F3).Field(t.F4).Finish()
}
