// Port of tracing-subscriber 0.3.23 src/fmt/writer.rs

package tracingsubscriber

import (
	"io"
	"reflect"

	"github.com/BrokkAi/mjolnir/go/crates/tracing"
	"github.com/BrokkAi/mjolnir/go/rs/rsio"
)

// FmtMakeWriter is tracing_subscriber::fmt::MakeWriter: what the fmt layer
// asks for a writer, once for each event. Rust's io::Write is io.Writer here
// (see rsio). An implementation that relies on the default make_writer_for
// forwards to FmtMakeWriterDefaultMakeWriterFor.
//
// The fmt layer drops the writer after writing the event, as Rust drops it:
// a writer with a Drop method has it called.
type FmtMakeWriter interface {
	// MakeWriter returns a writer for one event.
	MakeWriter() io.Writer
	// MakeWriterFor returns a writer for one event with this metadata.
	MakeWriterFor(meta *tracing.Metadata) io.Writer
}

// FmtMakeWriterDefaultMakeWriterFor is the default body of
// MakeWriter::make_writer_for: MakeWriter.
func FmtMakeWriterDefaultMakeWriterFor(self FmtMakeWriter, meta *tracing.Metadata) io.Writer {
	return self.MakeWriter()
}

// FmtMakeWriterFn is `impl MakeWriter for F where F: Fn() -> W`: a function
// that returns a writer is a MakeWriter.
type FmtMakeWriterFn func() io.Writer

// MakeWriter calls the function.
func (f FmtMakeWriterFn) MakeWriter() io.Writer { return f() }

// MakeWriterFor is the default make_writer_for.
func (f FmtMakeWriterFn) MakeWriterFor(meta *tracing.Metadata) io.Writer {
	return FmtMakeWriterDefaultMakeWriterFor(f, meta)
}

// intoMakeWriter accepts what `with_writer` accepts in Rust: a MakeWriter,
// or a function of no arguments that returns a writer (`io::stderr`,
// `move || file.try_clone().unwrap()`, `mj_core::logging::stderr_writer`),
// whatever its declared result type.
func intoMakeWriter(makeWriter any) FmtMakeWriter {
	switch makeWriter := makeWriter.(type) {
	case FmtMakeWriter:
		return makeWriter
	case func() io.Writer:
		return FmtMakeWriterFn(makeWriter)
	}
	value := reflect.ValueOf(makeWriter)
	writerType := reflect.TypeFor[io.Writer]()
	if value.Kind() == reflect.Func && !value.IsNil() && value.Type().NumIn() == 0 &&
		value.Type().NumOut() == 1 && value.Type().Out(0).Implements(writerType) {
		return FmtMakeWriterFn(func() io.Writer {
			return value.Call(nil)[0].Interface().(io.Writer)
		})
	}
	panic("tracing_subscriber: with_writer needs a fmt::MakeWriter or a function returning an io::Write, not " +
		reflect.TypeOf(makeWriter).String())
}

// stdoutMakeWriter is `io::stdout` as a MakeWriter: the default writer.
func stdoutMakeWriter() FmtMakeWriter {
	return FmtMakeWriterFn(func() io.Writer { return rsio.Stdout() })
}
