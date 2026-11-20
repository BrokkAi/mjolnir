// Port of alloc 1.99.1 src/fmt.rs (format), core 1.99.1 src/fmt/mod.rs
// (write) and src/fmt/rt.rs, and of the argument checks in rustc 1.99.1
// compiler/rustc_builtin_macros/src/format.rs.

package rs

import (
	"reflect"
	"runtime"
	"strconv"
	"strings"
)

// formatToString is alloc::fmt::format: `format!`.
func formatToString(format string, args []any) string {
	template := fmtParse(format)
	if s, ok := template.asStr(); ok && len(args) == 0 {
		return s
	}
	output := fmtString{buf: make([]byte, 0, template.estimatedCapacity())}
	if err := fmtWriteTemplate(&output, template, args); err != nil {
		panic(fmtPanicInfo("a formatting trait implementation returned an error when the underlying stream did not: Error"))
	}
	return string(output.buf)
}

// formatToStringForIo is the text `write!` sends to an io::Write
// (io::Write::write_fmt). It differs from formatToString only in the panic
// for a formatting method that fails although the writer did not.
func formatToStringForIo(format string, args []any) string {
	template := fmtParse(format)
	if s, ok := template.asStr(); ok && len(args) == 0 {
		return s
	}
	output := fmtString{buf: make([]byte, 0, template.estimatedCapacity())}
	if err := fmtWriteTemplate(&output, template, args); err != nil {
		panic(fmtPanicInfo("a formatting trait implementation returned an error when the underlying stream did not"))
	}
	return string(output.buf)
}

// fmtToString is `<T as ToString>::to_string`: the Display output of a value.
func fmtToString(value any) string {
	if s, ok := value.(string); ok {
		return s
	}
	var output fmtString
	formatter := fmtFormatterNew(&output, fmtFormattingOptionsNew())
	if err := fmtDisplay(value, formatter); err != nil {
		panic(fmtPanicInfo("a Display implementation returned an error unexpectedly: Error"))
	}
	return string(output.buf)
}

// FmtArguments is Rust's fmt::Arguments: a format string and its arguments,
// not yet written anywhere. Its Display and Debug output is the formatted
// text.
type FmtArguments struct {
	format string
	args   []any
}

// FormatArgs is `format_args!`.
func FormatArgs(format string, args ...any) FmtArguments {
	return FmtArguments{format: format, args: args}
}

// FmtDisplay is `impl Display for Arguments`.
func (a FmtArguments) FmtDisplay(f *Formatter) error {
	return fmtWriteTemplate(f.buf, fmtParse(a.format), a.args)
}

// FmtDebug is `impl Debug for Arguments`.
func (a FmtArguments) FmtDebug(f *Formatter) error {
	return fmtWriteTemplate(f.buf, fmtParse(a.format), a.args)
}

// String is `Arguments::to_string`.
func (a FmtArguments) String() string {
	return formatToString(a.format, a.args)
}

// fmtWriteTemplate is fmt::write: it writes the template's text to output,
// formatting an argument for every placeholder.
//
// What rustc checks about the arguments when it compiles a `format!` is
// checked here: every placeholder must have its argument, and every argument
// must be used.
func fmtWriteTemplate(output fmtWrite, template *fmtTemplate, args []any) error {
	// Positional arguments come first and named ones after them; a named
	// argument also has a position, as in Rust.
	firstNamed := len(args)
	for i, arg := range args {
		if _, named := arg.(NamedArg); named {
			if firstNamed == len(args) {
				firstNamed = i
			}
		} else if firstNamed != len(args) {
			panic(fmtArgumentsPanic(template, "positional arguments cannot follow named arguments"))
		}
	}
	for i := firstNamed; i < len(args); i++ {
		name := args[i].(NamedArg).Name
		for j := firstNamed; j < i; j++ {
			if args[j].(NamedArg).Name == name {
				panic(fmtArgumentsPanic(template, "duplicate argument named `"+name+"`"))
			}
		}
	}

	// used marks the arguments some placeholder read.
	var usedSmall [16]bool
	used := usedSmall[:0]
	if len(args) <= len(usedSmall) {
		used = usedSmall[:len(args)]
	} else {
		used = make([]bool, len(args))
	}

	byIndex := func(index int, implicit bool) any {
		if index >= len(args) {
			panic(fmtArgumentsPanic(template, fmtInvalidReference(template, index, implicit, len(args))))
		}
		used[index] = true
		if named, ok := args[index].(NamedArg); ok {
			return named.Value
		}
		return args[index]
	}
	byName := func(name string) any {
		for i := firstNamed; i < len(args); i++ {
			if named := args[i].(NamedArg); named.Name == name {
				used[i] = true
				return named.Value
			}
		}
		panic(fmtArgumentsPanic(template, "there is no argument named `"+name+"` (Rust captures it from the scope; pass it as rs.Arg(\""+name+"\", value))"))
	}
	count := func(c *fmtCount, what string) uint16 {
		switch c.kind {
		case fmtCountIs:
			return uint16(c.value)
		case fmtCountIsParam:
			return fmtUsizeArgument(template, byIndex(c.value, false), what)
		case fmtCountIsStar:
			return fmtUsizeArgument(template, byIndex(c.value, true), what)
		case fmtCountIsName:
			return fmtUsizeArgument(template, byName(c.name), what)
		default:
			return 0
		}
	}

	for i := range template.pieces {
		piece := &template.pieces[i]
		placeholder := piece.placeholder
		if placeholder == nil {
			if err := output.writeStr(piece.literal); err != nil {
				return err
			}
			continue
		}

		// rustc looks the arguments of a placeholder up in this order: the
		// value, the precision, the width.
		var value any
		switch placeholder.position {
		case fmtArgumentNamed:
			value = byName(placeholder.name)
		case fmtArgumentIs:
			value = byIndex(placeholder.index, false)
		default:
			value = byIndex(placeholder.index, true)
		}
		options := fmtFormattingOptions{flags: placeholder.flags}
		options.precision = count(&placeholder.precision, "precision")
		options.width = count(&placeholder.width, "width")

		formatter := Formatter{options: options, buf: output}
		if err := fmtFormatValue(placeholder.trait, value, &formatter); err != nil {
			return err
		}
	}

	for i, wasUsed := range used {
		if !wasUsed {
			if named, ok := args[i].(NamedArg); ok {
				panic(fmtArgumentsPanic(template, "named argument `"+named.Name+"` never used"))
			}
			panic(fmtArgumentsPanic(template, "argument "+strconv.Itoa(i)+" never used"))
		}
	}
	return nil
}

// fmtFormatValue formats value with the trait a placeholder names.
func fmtFormatValue(trait fmtTrait, value any, f *Formatter) error {
	switch trait {
	case fmtTraitDisplay:
		return fmtDisplay(value, f)
	case fmtTraitDebug:
		return fmtDebug(value, f)
	case fmtTraitLowerExp:
		return fmtExp(value, false, f)
	case fmtTraitUpperExp:
		return fmtExp(value, true, f)
	case fmtTraitOctal:
		return fmtRadixValue(value, 8, false, f)
	case fmtTraitPointer:
		return fmtPointer(value, f)
	case fmtTraitBinary:
		return fmtRadixValue(value, 2, false, f)
	case fmtTraitLowerHex:
		return fmtRadixValue(value, 16, false, f)
	default:
		return fmtRadixValue(value, 16, true, f)
	}
}

// fmtUsizeArgument reads a width or precision argument, which Rust requires
// to be a usize (rt::Argument::from_usize).
func fmtUsizeArgument(template *fmtTemplate, value any, what string) uint16 {
	var count uint64
	switch v := value.(type) {
	case int:
		if v < 0 {
			panic(fmtArgumentsPanic(template, "the "+what+" argument is negative"))
		}
		count = uint64(v)
	default:
		rv := reflect.ValueOf(value)
		switch rv.Kind() {
		case reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64:
			if rv.Int() < 0 {
				panic(fmtArgumentsPanic(template, "the "+what+" argument is negative"))
			}
			count = uint64(rv.Int())
		case reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64, reflect.Uintptr:
			count = rv.Uint()
		default:
			panic(fmtArgumentsPanic(template, "the "+what+" argument must be an integer (a usize in Rust), not "+fmtGoTypeName(rv)))
		}
	}
	if count > 0xFFFF {
		panic(fmtPanicInfo("Formatting argument out of range"))
	}
	return uint16(count)
}

// fmtInvalidReference words the error for a placeholder whose argument does
// not exist, as rustc does.
func fmtInvalidReference(template *fmtTemplate, index int, implicit bool, given int) string {
	var there string
	switch given {
	case 0:
		there = "no arguments were given"
	case 1:
		there = "there is 1 argument"
	default:
		there = "there are " + strconv.Itoa(given) + " arguments"
	}
	if implicit {
		placeholders := 0
		for i := range template.pieces {
			if p := template.pieces[i].placeholder; p != nil {
				if p.position == fmtArgumentImplicitlyIs {
					placeholders = max(placeholders, p.index+1)
				}
				if p.precision.kind == fmtCountIsStar {
					placeholders = max(placeholders, p.precision.value+1)
				}
			}
		}
		plural := "s"
		if placeholders == 1 {
			plural = ""
		}
		if given == 0 {
			return strconv.Itoa(placeholders) + " positional argument" + plural + " in format string, but " + there
		}
		return strconv.Itoa(placeholders) + " positional argument" + plural + " in format string, but " + there
	}
	return "invalid reference to positional argument " + strconv.Itoa(index) + " (" + there + ")"
}

func fmtArgumentsPanic(template *fmtTemplate, description string) *PanicInfo {
	return fmtPanicInfo(description + ": format string " + strconv.Quote(template.source))
}

// fmtPackagePrefix is how the runtime names the functions of this package.
var fmtPackagePrefix = reflect.TypeFor[Formatter]().PkgPath() + "."

// fmtPanicInfo builds a panic raised by the formatter, attributed to the
// first caller outside this package: the line that called rs.Format.
func fmtPanicInfo(message string) *PanicInfo {
	info := &PanicInfo{Message: message}
	var pcs [48]uintptr
	n := runtime.Callers(2, pcs[:])
	frames := runtime.CallersFrames(pcs[:n])
	for {
		frame, more := frames.Next()
		if !strings.HasPrefix(frame.Function, fmtPackagePrefix) || strings.HasSuffix(frame.File, "_test.go") {
			info.File = frame.File
			info.Line = frame.Line
			break
		}
		if !more {
			break
		}
	}
	return info
}
