// Port of vte 0.15.0 src/lib.rs

package vte

import (
	"bytes"
	"reflect"
	"strings"
	"testing"
)

var oscBytes = []uint8{
	0x1B, 0x5D, // Begin OSC
	'2', ';', 'j', 'w', 'i', 'l', 'm', '@', 'j', 'w', 'i', 'l', 'm', '-', 'd',
	'e', 's', 'k', ':', ' ', '~', '/', 'c', 'o', 'd', 'e', '/', 'a', 'l', 'a',
	'c', 'r', 'i', 't', 't', 'y', 0x07, // End OSC
}

type dispatcher struct {
	dispatched []sequence
}

type sequence interface{ isSequence() }

type sequence_Osc struct {
	F0 [][]uint8
	F1 bool
}
type sequence_Csi struct {
	F0 [][]uint16
	F1 []uint8
	F2 bool
	F3 rune
}
type sequence_Esc struct {
	F0 []uint8
	F1 bool
	F2 uint8
}
type sequence_DcsHook struct {
	F0 [][]uint16
	F1 []uint8
	F2 bool
	F3 rune
}
type sequence_DcsPut struct{ F0 uint8 }
type sequence_Print struct{ F0 rune }
type sequence_Execute struct{ F0 uint8 }
type sequence_DcsUnhook struct{}

func (*sequence_Osc) isSequence()       {}
func (*sequence_Csi) isSequence()       {}
func (*sequence_Esc) isSequence()       {}
func (*sequence_DcsHook) isSequence()   {}
func (*sequence_DcsPut) isSequence()    {}
func (*sequence_Print) isSequence()     {}
func (*sequence_Execute) isSequence()   {}
func (*sequence_DcsUnhook) isSequence() {}

// paramsToVec is `params.iter().map(|subparam| subparam.to_vec()).collect()`.
func paramsToVec(params *Params) [][]uint16 {
	collected := [][]uint16{}
	for iter := params.Iter(); ; {
		subparam, ok := iter.Next().Get()
		if !ok {
			break
		}
		collected = append(collected, append([]uint16{}, subparam...))
	}
	return collected
}

func (self *dispatcher) OscDispatch(params [][]uint8, bellTerminated bool) {
	collected := [][]uint8{}
	for _, p := range params {
		collected = append(collected, append([]uint8{}, p...))
	}
	self.dispatched = append(self.dispatched, &sequence_Osc{F0: collected, F1: bellTerminated})
}

func (self *dispatcher) CsiDispatch(params *Params, intermediates []uint8, ignore bool, c rune) {
	collected := paramsToVec(params)
	intermediates = append([]uint8{}, intermediates...)
	self.dispatched = append(self.dispatched, &sequence_Csi{F0: collected, F1: intermediates, F2: ignore, F3: c})
}

func (self *dispatcher) EscDispatch(intermediates []uint8, ignore bool, byte_ uint8) {
	intermediates = append([]uint8{}, intermediates...)
	self.dispatched = append(self.dispatched, &sequence_Esc{F0: intermediates, F1: ignore, F2: byte_})
}

func (self *dispatcher) Hook(params *Params, intermediates []uint8, ignore bool, c rune) {
	collected := paramsToVec(params)
	intermediates = append([]uint8{}, intermediates...)
	self.dispatched = append(self.dispatched, &sequence_DcsHook{F0: collected, F1: intermediates, F2: ignore, F3: c})
}

func (self *dispatcher) Put(byte_ uint8) {
	self.dispatched = append(self.dispatched, &sequence_DcsPut{F0: byte_})
}

func (self *dispatcher) Unhook() {
	self.dispatched = append(self.dispatched, &sequence_DcsUnhook{})
}

func (self *dispatcher) Print(c rune) {
	self.dispatched = append(self.dispatched, &sequence_Print{F0: c})
}

func (self *dispatcher) Execute(byte_ uint8) {
	self.dispatched = append(self.dispatched, &sequence_Execute{F0: byte_})
}

func (self *dispatcher) Terminated() bool { return PerformDefaultTerminated(self) }

// assertEq is `assert_eq!` for these tests. A nil slice equals an empty one,
// as two empty Vecs are equal.
func assertEq(t *testing.T, left, right any) {
	t.Helper()
	if !reflect.DeepEqual(normalize(reflect.ValueOf(left)).Interface(), normalize(reflect.ValueOf(right)).Interface()) {
		t.Fatalf("assertion `left == right` failed\n  left: %#v\n right: %#v", left, right)
	}
}

// normalize returns a copy of value in which every nil slice is empty.
func normalize(value reflect.Value) reflect.Value {
	switch value.Kind() {
	case reflect.Slice:
		out := reflect.MakeSlice(value.Type(), value.Len(), value.Len())
		for i := 0; i < value.Len(); i++ {
			out.Index(i).Set(normalize(value.Index(i)))
		}
		return out
	case reflect.Pointer:
		if value.IsNil() {
			return value
		}
		out := reflect.New(value.Type().Elem())
		out.Elem().Set(normalize(value.Elem()))
		return out
	case reflect.Struct:
		out := reflect.New(value.Type()).Elem()
		for i := 0; i < value.NumField(); i++ {
			out.Field(i).Set(normalize(value.Field(i)))
		}
		return out
	case reflect.Interface:
		if value.IsNil() {
			return value
		}
		out := reflect.New(value.Type()).Elem()
		out.Set(normalize(value.Elem()))
		return out
	default:
		return value
	}
}

func assert(t *testing.T, cond bool) {
	t.Helper()
	if !cond {
		t.Fatal("assertion failed")
	}
}

func TestLib_parse_osc(t *testing.T) {
	dispatcher := dispatcher{}
	parser := ParserNew()

	parser.Advance(&dispatcher, oscBytes)

	assertEq(t, len(dispatcher.dispatched), 1)
	switch seq := dispatcher.dispatched[0].(type) {
	case *sequence_Osc:
		params := seq.F0
		assertEq(t, len(params), 2)
		assertEq(t, params[0], oscBytes[2:3])
		assertEq(t, params[1], oscBytes[4:(len(oscBytes)-1)])
	default:
		t.Fatal("expected osc sequence")
	}
}

func TestLib_parse_empty_osc(t *testing.T) {
	dispatcher := dispatcher{}
	parser := ParserNew()

	parser.Advance(&dispatcher, []uint8{0x1B, 0x5D, 0x07})

	assertEq(t, len(dispatcher.dispatched), 1)
	switch dispatcher.dispatched[0].(type) {
	case *sequence_Osc:
	default:
		t.Fatal("expected osc sequence")
	}
}

func TestLib_parse_osc_max_params(t *testing.T) {
	params := strings.Repeat(";", paramsMaxParams+1)
	input := []uint8("\x1b]" + params + "\x1b")
	dispatcher := dispatcher{}
	parser := ParserNew()

	parser.Advance(&dispatcher, input)

	assertEq(t, len(dispatcher.dispatched), 1)
	switch seq := dispatcher.dispatched[0].(type) {
	case *sequence_Osc:
		params := seq.F0
		assertEq(t, len(params), maxOscParams)
		for _, param := range params {
			assert(t, len(param) == 0)
		}
	default:
		t.Fatal("expected osc sequence")
	}
}

func TestLib_osc_bell_terminated(t *testing.T) {
	input := []uint8("\x1b]11;ff/00/ff\x07")
	dispatcher := dispatcher{}
	parser := ParserNew()

	parser.Advance(&dispatcher, input)

	assertEq(t, len(dispatcher.dispatched), 1)
	switch seq := dispatcher.dispatched[0].(type) {
	case *sequence_Osc:
		if !seq.F1 {
			t.Fatal("expected osc with bell terminator")
		}
	default:
		t.Fatal("expected osc with bell terminator")
	}
}

func TestLib_osc_c0_st_terminated(t *testing.T) {
	input := []uint8("\x1b]11;ff/00/ff\x1b\\")
	dispatcher := dispatcher{}
	parser := ParserNew()

	parser.Advance(&dispatcher, input)

	assertEq(t, len(dispatcher.dispatched), 2)
	switch seq := dispatcher.dispatched[0].(type) {
	case *sequence_Osc:
		if seq.F1 {
			t.Fatal("expected osc with ST terminator")
		}
	default:
		t.Fatal("expected osc with ST terminator")
	}
}

func TestLib_parse_osc_with_utf8_arguments(t *testing.T) {
	input := []uint8{
		0x0D, 0x1B, 0x5D, 0x32, 0x3B, 0x65, 0x63, 0x68, 0x6F, 0x20, 0x27, 0xC2, 0xAF, 0x5C,
		0x5F, 0x28, 0xE3, 0x83, 0x84, 0x29, 0x5F, 0x2F, 0xC2, 0xAF, 0x27, 0x20, 0x26, 0x26,
		0x20, 0x73, 0x6C, 0x65, 0x65, 0x70, 0x20, 0x31, 0x07,
	}
	dispatcher := dispatcher{}
	parser := ParserNew()

	parser.Advance(&dispatcher, input)

	assertEq(t, dispatcher.dispatched[0], sequence(&sequence_Execute{F0: '\r'}))
	oscData := append([]uint8{}, input[5:(len(input)-1)]...)
	assertEq(t, dispatcher.dispatched[1], sequence(&sequence_Osc{F0: [][]uint8{{'2'}, oscData}, F1: true}))
	assertEq(t, len(dispatcher.dispatched), 2)
}

func TestLib_osc_containing_string_terminator(t *testing.T) {
	input := []uint8("\x1b]2;\xe6\x9c\xab\x1b\\")
	dispatcher := dispatcher{}
	parser := ParserNew()

	parser.Advance(&dispatcher, input)

	assertEq(t, len(dispatcher.dispatched), 2)
	switch seq := dispatcher.dispatched[0].(type) {
	case *sequence_Osc:
		params := seq.F0
		assertEq(t, params[1], input[4:(len(input)-2)])
	default:
		t.Fatal("expected osc sequence")
	}
}

func TestLib_exceed_max_buffer_size(t *testing.T) {
	const numBytes = maxOscRaw + 100
	inputStart := []uint8("\x1b]52;s")
	inputEnd := []uint8("\x07")

	dispatcher := dispatcher{}
	parser := ParserNew()

	// Create valid OSC escape
	parser.Advance(&dispatcher, inputStart)

	// Exceed max buffer size
	parser.Advance(&dispatcher, bytes.Repeat([]uint8{'a'}, numBytes))

	// Terminate escape for dispatch
	parser.Advance(&dispatcher, inputEnd)

	assertEq(t, len(dispatcher.dispatched), 1)
	switch seq := dispatcher.dispatched[0].(type) {
	case *sequence_Osc:
		params := seq.F0
		assertEq(t, len(params), 2)
		assertEq(t, params[0], []uint8("52"))

		// This is the crate built with its `std` feature, where the buffer
		// is not limited.
		assertEq(t, len(params[1]), numBytes+len(inputEnd))
	default:
		t.Fatal("expected osc sequence")
	}
}

func TestLib_parse_csi_max_params(t *testing.T) {
	// This will build a list of repeating '1;'s
	// The length is MAX_PARAMS - 1 because the last semicolon is interpreted
	// as an implicit zero, making the total number of parameters MAX_PARAMS
	params := strings.Repeat("1;", paramsMaxParams-1)
	input := []uint8("\x1b[" + params + "p")

	dispatcher := dispatcher{}
	parser := ParserNew()

	parser.Advance(&dispatcher, input)

	assertEq(t, len(dispatcher.dispatched), 1)
	switch seq := dispatcher.dispatched[0].(type) {
	case *sequence_Csi:
		params, ignore := seq.F0, seq.F2
		assertEq(t, len(params), paramsMaxParams)
		assert(t, !ignore)
	default:
		t.Fatal("expected csi sequence")
	}
}

func TestLib_parse_csi_params_ignore_long_params(t *testing.T) {
	// This will build a list of repeating '1;'s
	// The length is MAX_PARAMS because the last semicolon is interpreted
	// as an implicit zero, making the total number of parameters MAX_PARAMS + 1
	params := strings.Repeat("1;", paramsMaxParams)
	input := []uint8("\x1b[" + params + "p")

	dispatcher := dispatcher{}
	parser := ParserNew()

	parser.Advance(&dispatcher, input)

	assertEq(t, len(dispatcher.dispatched), 1)
	switch seq := dispatcher.dispatched[0].(type) {
	case *sequence_Csi:
		params, ignore := seq.F0, seq.F2
		assertEq(t, len(params), paramsMaxParams)
		assert(t, ignore)
	default:
		t.Fatal("expected csi sequence")
	}
}

func TestLib_parse_csi_params_trailing_semicolon(t *testing.T) {
	dispatcher := dispatcher{}
	parser := ParserNew()

	parser.Advance(&dispatcher, []uint8("\x1b[4;m"))

	assertEq(t, len(dispatcher.dispatched), 1)
	switch seq := dispatcher.dispatched[0].(type) {
	case *sequence_Csi:
		assertEq(t, seq.F0, [][]uint16{{4}, {0}})
	default:
		t.Fatal("expected csi sequence")
	}
}

func TestLib_parse_csi_params_leading_semicolon(t *testing.T) {
	// Create dispatcher and check state
	dispatcher := dispatcher{}
	parser := ParserNew()

	parser.Advance(&dispatcher, []uint8("\x1b[;4m"))

	assertEq(t, len(dispatcher.dispatched), 1)
	switch seq := dispatcher.dispatched[0].(type) {
	case *sequence_Csi:
		assertEq(t, seq.F0, [][]uint16{{0}, {4}})
	default:
		t.Fatal("expected csi sequence")
	}
}

func TestLib_parse_long_csi_param(t *testing.T) {
	// The important part is the parameter, which is (i64::MAX + 1)
	input := []uint8("\x1b[9223372036854775808m")
	dispatcher := dispatcher{}
	parser := ParserNew()

	parser.Advance(&dispatcher, input)

	assertEq(t, len(dispatcher.dispatched), 1)
	switch seq := dispatcher.dispatched[0].(type) {
	case *sequence_Csi:
		assertEq(t, seq.F0, [][]uint16{{0xFFFF}})
	default:
		t.Fatal("expected csi sequence")
	}
}

func TestLib_csi_reset(t *testing.T) {
	input := []uint8("\x1b[3;1\x1b[?1049h")
	dispatcher := dispatcher{}
	parser := ParserNew()

	parser.Advance(&dispatcher, input)

	assertEq(t, len(dispatcher.dispatched), 1)
	switch seq := dispatcher.dispatched[0].(type) {
	case *sequence_Csi:
		params, intermediates, ignore := seq.F0, seq.F1, seq.F2
		assertEq(t, intermediates, []uint8("?"))
		assertEq(t, params, [][]uint16{{1049}})
		assert(t, !ignore)
	default:
		t.Fatal("expected csi sequence")
	}
}

func TestLib_csi_subparameters(t *testing.T) {
	input := []uint8("\x1b[38:2:255:0:255;1m")
	dispatcher := dispatcher{}
	parser := ParserNew()

	parser.Advance(&dispatcher, input)

	assertEq(t, len(dispatcher.dispatched), 1)
	switch seq := dispatcher.dispatched[0].(type) {
	case *sequence_Csi:
		params, intermediates, ignore := seq.F0, seq.F1, seq.F2
		assertEq(t, params, [][]uint16{{38, 2, 255, 0, 255}, {1}})
		assertEq(t, intermediates, []uint8{})
		assert(t, !ignore)
	default:
		t.Fatal("expected csi sequence")
	}
}

func TestLib_parse_dcs_max_params(t *testing.T) {
	params := strings.Repeat("1;", paramsMaxParams+1)
	input := []uint8("\x1bP" + params + "p")
	dispatcher := dispatcher{}
	parser := ParserNew()

	parser.Advance(&dispatcher, input)

	assertEq(t, len(dispatcher.dispatched), 1)
	switch seq := dispatcher.dispatched[0].(type) {
	case *sequence_DcsHook:
		params, ignore := seq.F0, seq.F2
		assertEq(t, len(params), paramsMaxParams)
		for _, param := range params {
			assertEq(t, param, []uint16{1})
		}
		assert(t, ignore)
	default:
		t.Fatal("expected dcs sequence")
	}
}

func TestLib_dcs_reset(t *testing.T) {
	input := []uint8("\x1b[3;1\x1bP1$tx\x9c")
	dispatcher := dispatcher{}
	parser := ParserNew()

	parser.Advance(&dispatcher, input)

	assertEq(t, len(dispatcher.dispatched), 3)
	switch seq := dispatcher.dispatched[0].(type) {
	case *sequence_DcsHook:
		params, intermediates, ignore := seq.F0, seq.F1, seq.F2
		assertEq(t, intermediates, []uint8("$"))
		assertEq(t, params, [][]uint16{{1}})
		assert(t, !ignore)
	default:
		t.Fatal("expected dcs sequence")
	}
	assertEq(t, dispatcher.dispatched[1], sequence(&sequence_DcsPut{F0: 'x'}))
	assertEq(t, dispatcher.dispatched[2], sequence(&sequence_DcsUnhook{}))
}

func TestLib_parse_dcs(t *testing.T) {
	input := []uint8("\x1bP0;1|17/ab\x9c")
	dispatcher := dispatcher{}
	parser := ParserNew()

	parser.Advance(&dispatcher, input)

	assertEq(t, len(dispatcher.dispatched), 7)
	switch seq := dispatcher.dispatched[0].(type) {
	case *sequence_DcsHook:
		params, c := seq.F0, seq.F3
		assertEq(t, params, [][]uint16{{0}, {1}})
		assertEq(t, c, '|')
	default:
		t.Fatal("expected dcs sequence")
	}
	for i, byte_ := range []uint8("17/ab") {
		assertEq(t, dispatcher.dispatched[1+i], sequence(&sequence_DcsPut{F0: byte_}))
	}
	assertEq(t, dispatcher.dispatched[6], sequence(&sequence_DcsUnhook{}))
}

func TestLib_intermediate_reset_on_dcs_exit(t *testing.T) {
	input := []uint8("\x1bP=1sZZZ\x1b+\x5c")
	dispatcher := dispatcher{}
	parser := ParserNew()

	parser.Advance(&dispatcher, input)

	assertEq(t, len(dispatcher.dispatched), 6)
	switch seq := dispatcher.dispatched[5].(type) {
	case *sequence_Esc:
		assertEq(t, seq.F0, []uint8("+"))
	default:
		t.Fatal("expected esc sequence")
	}
}

func TestLib_esc_reset(t *testing.T) {
	input := []uint8("\x1b[3;1\x1b(A")
	dispatcher := dispatcher{}
	parser := ParserNew()

	parser.Advance(&dispatcher, input)

	assertEq(t, len(dispatcher.dispatched), 1)
	switch seq := dispatcher.dispatched[0].(type) {
	case *sequence_Esc:
		intermediates, ignore, byte_ := seq.F0, seq.F1, seq.F2
		assertEq(t, intermediates, []uint8("("))
		assertEq(t, byte_, uint8('A'))
		assert(t, !ignore)
	default:
		t.Fatal("expected esc sequence")
	}
}

func TestLib_esc_reset_intermediates(t *testing.T) {
	input := []uint8("\x1b[?2004l\x1b#8")
	dispatcher := dispatcher{}
	parser := ParserNew()

	parser.Advance(&dispatcher, input)

	assertEq(t, len(dispatcher.dispatched), 2)
	assertEq(t, dispatcher.dispatched[0], sequence(&sequence_Csi{F0: [][]uint16{{2004}}, F1: []uint8{63}, F2: false, F3: 'l'}))
	assertEq(t, dispatcher.dispatched[1], sequence(&sequence_Esc{F0: []uint8{35}, F1: false, F2: 56}))
}

func TestLib_params_buffer_filled_with_subparam(t *testing.T) {
	input := []uint8("\x1b[::::::::::::::::::::::::::::::::x\x1b")
	dispatcher := dispatcher{}
	parser := ParserNew()

	parser.Advance(&dispatcher, input)

	assertEq(t, len(dispatcher.dispatched), 1)
	switch seq := dispatcher.dispatched[0].(type) {
	case *sequence_Csi:
		params, intermediates, ignore, c := seq.F0, seq.F1, seq.F2, seq.F3
		assertEq(t, intermediates, []uint8{})
		assertEq(t, params, [][]uint16{make([]uint16, 32)})
		assertEq(t, c, 'x')
		assert(t, ignore)
	default:
		t.Fatal("expected csi sequence")
	}
}

func TestLib_unicode(t *testing.T) {
	input := []uint8("\xF0\x9F\x8E\x89_\xF0\x9F\xA6\x80\xF0\x9F\xA6\x80_\xF0\x9F\x8E\x89")

	dispatcher := dispatcher{}
	parser := ParserNew()

	parser.Advance(&dispatcher, input)

	assertEq(t, len(dispatcher.dispatched), 6)
	assertEq(t, dispatcher.dispatched[0], sequence(&sequence_Print{F0: '🎉'}))
	assertEq(t, dispatcher.dispatched[1], sequence(&sequence_Print{F0: '_'}))
	assertEq(t, dispatcher.dispatched[2], sequence(&sequence_Print{F0: '🦀'}))
	assertEq(t, dispatcher.dispatched[3], sequence(&sequence_Print{F0: '🦀'}))
	assertEq(t, dispatcher.dispatched[4], sequence(&sequence_Print{F0: '_'}))
	assertEq(t, dispatcher.dispatched[5], sequence(&sequence_Print{F0: '🎉'}))
}

func TestLib_invalid_utf8(t *testing.T) {
	input := []uint8("a\xEF\xBCb")

	dispatcher := dispatcher{}
	parser := ParserNew()

	parser.Advance(&dispatcher, input)

	assertEq(t, len(dispatcher.dispatched), 3)
	assertEq(t, dispatcher.dispatched[0], sequence(&sequence_Print{F0: 'a'}))
	assertEq(t, dispatcher.dispatched[1], sequence(&sequence_Print{F0: '�'}))
	assertEq(t, dispatcher.dispatched[2], sequence(&sequence_Print{F0: 'b'}))
}

func TestLib_partial_utf8(t *testing.T) {
	input := []uint8("\xF0\x9F\x9A\x80")

	dispatcher := dispatcher{}
	parser := ParserNew()

	parser.Advance(&dispatcher, input[:1])
	parser.Advance(&dispatcher, input[1:2])
	parser.Advance(&dispatcher, input[2:3])
	parser.Advance(&dispatcher, input[3:])

	assertEq(t, len(dispatcher.dispatched), 1)
	assertEq(t, dispatcher.dispatched[0], sequence(&sequence_Print{F0: '🚀'}))
}

func TestLib_partial_utf8_separating_utf8(t *testing.T) {
	// This is different from the `partial_utf8` test since it has a multi-byte UTF8
	// character after the partial UTF8 state, causing a partial byte to be present
	// in the `partial_utf8` buffer after the 2-byte codepoint.

	// "ĸ🎉"
	input := []uint8("\xC4\xB8\xF0\x9F\x8E\x89")

	dispatcher := dispatcher{}
	parser := ParserNew()

	parser.Advance(&dispatcher, input[:1])
	parser.Advance(&dispatcher, input[1:])

	assertEq(t, len(dispatcher.dispatched), 2)
	assertEq(t, dispatcher.dispatched[0], sequence(&sequence_Print{F0: 'ĸ'}))
	assertEq(t, dispatcher.dispatched[1], sequence(&sequence_Print{F0: '🎉'}))
}

func TestLib_partial_invalid_utf8(t *testing.T) {
	input := []uint8("a\xEF\xBCb")

	dispatcher := dispatcher{}
	parser := ParserNew()

	parser.Advance(&dispatcher, input[:1])
	parser.Advance(&dispatcher, input[1:2])
	parser.Advance(&dispatcher, input[2:3])
	parser.Advance(&dispatcher, input[3:])

	assertEq(t, len(dispatcher.dispatched), 3)
	assertEq(t, dispatcher.dispatched[0], sequence(&sequence_Print{F0: 'a'}))
	assertEq(t, dispatcher.dispatched[1], sequence(&sequence_Print{F0: '�'}))
	assertEq(t, dispatcher.dispatched[2], sequence(&sequence_Print{F0: 'b'}))
}

func TestLib_partial_invalid_utf8_split(t *testing.T) {
	input := []uint8("\xE4\xBF\x99\xB5")

	dispatcher := dispatcher{}
	parser := ParserNew()

	parser.Advance(&dispatcher, input[:2])
	parser.Advance(&dispatcher, input[2:])

	assertEq(t, dispatcher.dispatched[0], sequence(&sequence_Print{F0: '俙'}))
	assertEq(t, dispatcher.dispatched[1], sequence(&sequence_Print{F0: '�'}))
}

func TestLib_partial_utf8_into_esc(t *testing.T) {
	input := []uint8("\xD8\x1b012")

	dispatcher := dispatcher{}
	parser := ParserNew()

	parser.Advance(&dispatcher, input)

	assertEq(t, len(dispatcher.dispatched), 4)
	assertEq(t, dispatcher.dispatched[0], sequence(&sequence_Print{F0: '�'}))
	assertEq(t, dispatcher.dispatched[1], sequence(&sequence_Esc{F0: []uint8{}, F1: false, F2: '0'}))
	assertEq(t, dispatcher.dispatched[2], sequence(&sequence_Print{F0: '1'}))
	assertEq(t, dispatcher.dispatched[3], sequence(&sequence_Print{F0: '2'}))
}

func TestLib_c1s(t *testing.T) {
	input := []uint8("\x00\x1f\x80\x90\x98\x9b\x9c\x9d\x9e\x9fa")

	dispatcher := dispatcher{}
	parser := ParserNew()

	parser.Advance(&dispatcher, input)

	assertEq(t, len(dispatcher.dispatched), 11)
	assertEq(t, dispatcher.dispatched[0], sequence(&sequence_Execute{F0: 0}))
	assertEq(t, dispatcher.dispatched[1], sequence(&sequence_Execute{F0: 31}))
	assertEq(t, dispatcher.dispatched[2], sequence(&sequence_Execute{F0: 128}))
	assertEq(t, dispatcher.dispatched[3], sequence(&sequence_Execute{F0: 144}))
	assertEq(t, dispatcher.dispatched[4], sequence(&sequence_Execute{F0: 152}))
	assertEq(t, dispatcher.dispatched[5], sequence(&sequence_Execute{F0: 155}))
	assertEq(t, dispatcher.dispatched[6], sequence(&sequence_Execute{F0: 156}))
	assertEq(t, dispatcher.dispatched[7], sequence(&sequence_Execute{F0: 157}))
	assertEq(t, dispatcher.dispatched[8], sequence(&sequence_Execute{F0: 158}))
	assertEq(t, dispatcher.dispatched[9], sequence(&sequence_Execute{F0: 159}))
	assertEq(t, dispatcher.dispatched[10], sequence(&sequence_Print{F0: 'a'}))
}

func TestLib_execute_anywhere(t *testing.T) {
	input := []uint8("\x18\x1a")

	dispatcher := dispatcher{}
	parser := ParserNew()

	parser.Advance(&dispatcher, input)

	assertEq(t, len(dispatcher.dispatched), 2)
	assertEq(t, dispatcher.dispatched[0], sequence(&sequence_Execute{F0: 0x18}))
	assertEq(t, dispatcher.dispatched[1], sequence(&sequence_Execute{F0: 0x1A}))
}
