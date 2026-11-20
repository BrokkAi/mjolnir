package rs_test

import (
	"encoding/json"
	"fmt"
	"hash/fnv"
	"math"
	"math/big"
	"reflect"
	"strconv"
	"time"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// The types below are declared in Go the way the port declares the types of
// go/port/oracle/src/std_fmt.rs, so that the derived Debug the formatter
// prints by reflection can be held against the one rustc derives.

type oracleUnitStruct struct{}

var _ = rs.RegisterUnitStruct[oracleUnitStruct]("UnitStruct")

type oracleEmpty struct{}

var _ = rs.RegisterStruct[oracleEmpty]("Empty")

type oraclePoint struct {
	X int32 `rs:"x"`
	Y int32 `rs:"y"`
}

var _ = rs.RegisterStruct[oraclePoint]("Point")

type oraclePair struct {
	F0 any
	F1 any
}

var _ = rs.RegisterTupleStruct[oraclePair]("Pair")

type oracleWrap struct {
	F0 any
}

var _ = rs.RegisterTupleStruct[oracleWrap]("Wrap")

type oracleSessionId string

var _ = rs.RegisterNewtype[oracleSessionId]("SessionId")

type oracleMeters float64

var _ = rs.RegisterNewtype[oracleMeters]("Meters")

type oracleCount uint32

var _ = rs.RegisterNewtype[oracleCount]("Count")

type oracleTags []string

var _ = rs.RegisterNewtype[oracleTags]("Tags")

type oracleKind uint8

const (
	oracleKind_Alpha oracleKind = iota
	oracleKind_Beta
	oracleKind_GammaDelta
)

var _ = rs.RegisterEnum[oracleKind]("Kind",
	rs.Const(oracleKind_Alpha, "Alpha"),
	rs.Const(oracleKind_Beta, "Beta"),
	rs.Const(oracleKind_GammaDelta, "GammaDelta"),
)

type oracleState interface{ isOracleState() }

type oracleState_Idle struct{}

type oracleState_Running struct {
	Pid   uint32        `rs:"pid"`
	Since time.Duration `rs:"since"`
}

type oracleState_Failed struct {
	F0 string
	F1 int32
}

type oracleState_Wrapped struct {
	F0 oracleState
}

type oracleState_Maybe struct {
	F0 rs.Option[uint8]
}

type oracleState_Payload struct {
	F0 any
}

func (*oracleState_Idle) isOracleState()    {}
func (*oracleState_Running) isOracleState() {}
func (*oracleState_Failed) isOracleState()  {}
func (*oracleState_Wrapped) isOracleState() {}
func (*oracleState_Maybe) isOracleState()   {}
func (*oracleState_Payload) isOracleState() {}

var _ = rs.RegisterEnum[oracleState]("State",
	rs.Variant[oracleState_Idle]("Idle", rs.ShapeUnit),
	rs.Variant[oracleState_Running]("Running", rs.ShapeStruct),
	rs.Variant[oracleState_Failed]("Failed", rs.ShapeTuple),
	rs.Variant[oracleState_Wrapped]("Wrapped", rs.ShapeTuple),
	rs.Variant[oracleState_Maybe]("Maybe", rs.ShapeTuple),
	rs.Variant[oracleState_Payload]("Payload", rs.ShapeTuple),
)

type oracleRecord struct {
	Id      uint64                   `rs:"id"`
	Name    string                   `rs:"name"`
	Tags    []string                 `rs:"tags"`
	Parent  rs.Option[*oracleRecord] `rs:"parent"`
	Scores  map[string]float64       `rs:"scores"`
	Kind    oracleKind               `rs:"kind"`
	State   oracleState              `rs:"state"`
	Raw     []byte                   `rs:"raw"`
	Elapsed time.Duration            `rs:"elapsed"`
	Pair    rs.Tuple2[int32, string] `rs:"pair"`
	Ch      rs.Char                  `rs:"ch"`
	Unit    rs.Unit                  `rs:"unit"`
	Flag    bool                     `rs:"flag"`
	Ratio   float32                  `rs:"ratio"`
	Session oracleSessionId          `rs:"session"`
	// A private Rust field is an unexported Go field.
	privateNote rs.Option[string] `rs:"private_note"`
	Any         any               `rs:"any"`
}

var _ = rs.RegisterStruct[oracleRecord]("Record")

// oracleParseFailure is an error whose Debug is derived.
type oracleParseFailure struct {
	Line   uint32 `rs:"line"`
	Reason string `rs:"reason"`
}

var _ = rs.RegisterStruct[oracleParseFailure]("ParseFailure")

func (e *oracleParseFailure) Error() string { return "parse failure: " + e.Reason }

// oracleSeq is a sequence or a set that describes itself to the formatter the
// way the collections of go/rs/collections do.
type oracleSeq struct {
	items  []any
	set    bool
	sorted bool
}

func (s *oracleSeq) ReflectLen() int               { return len(s.items) }
func (s *oracleSeq) ReflectElemType() reflect.Type { return reflect.TypeFor[any]() }
func (s *oracleSeq) ReflectPush(element any)       { s.items = append(s.items, element) }
func (s *oracleSeq) ReflectIsSet() bool            { return s.set }
func (s *oracleSeq) ReflectSorted() bool           { return s.sorted }
func (s *oracleSeq) ReflectElements(yield func(element any) bool) {
	for i := range s.items {
		if !yield(&s.items[i]) {
			return
		}
	}
}

// oracleMap is a map that describes itself to the formatter.
type oracleMap struct {
	keys   []any
	values []any
	sorted bool
}

func (m *oracleMap) ReflectLen() int                { return len(m.keys) }
func (m *oracleMap) ReflectKeyType() reflect.Type   { return reflect.TypeFor[any]() }
func (m *oracleMap) ReflectValueType() reflect.Type { return reflect.TypeFor[any]() }
func (m *oracleMap) ReflectSorted() bool            { return m.sorted }
func (m *oracleMap) ReflectInsert(key any, value any) {
	m.keys, m.values = append(m.keys, key), append(m.values, value)
}
func (m *oracleMap) ReflectEntries(yield func(key any, value any) bool) {
	for i := range m.keys {
		if !yield(m.keys[i], &m.values[i]) {
			return
		}
	}
}

// oracleVariation picks, from the text of a value, one of n ways to hold it
// in Go, so that the choice is the same on every run.
func oracleVariation(raw json.RawMessage, n int) int {
	hash := fnv.New32a()
	hash.Write(raw)
	return int(hash.Sum32() % uint32(n))
}

func oracleParts(raw json.RawMessage) []json.RawMessage {
	var parts []json.RawMessage
	if err := json.Unmarshal(raw, &parts); err != nil {
		panic(fmt.Sprintf("oracle value is not an array: %s", raw))
	}
	return parts
}

func oracleText(raw json.RawMessage) string {
	var text string
	if err := json.Unmarshal(raw, &text); err != nil {
		panic(fmt.Sprintf("oracle value is not a string: %s", raw))
	}
	return text
}

func oracleInt(raw json.RawMessage) int64 {
	var n int64
	if err := json.Unmarshal(raw, &n); err != nil {
		panic(fmt.Sprintf("oracle value is not a number: %s", raw))
	}
	return n
}

func oracleSigned(raw json.RawMessage, bits int) int64 {
	n, err := strconv.ParseInt(oracleText(raw), 10, bits)
	if err != nil {
		panic(err)
	}
	return n
}

func oracleUnsigned(raw json.RawMessage, bits int) uint64 {
	n, err := strconv.ParseUint(oracleText(raw), 10, bits)
	if err != nil {
		panic(err)
	}
	return n
}

// oracleWide builds a 128-bit integer from its decimal text.
func oracleWide(raw json.RawMessage, signed bool) any {
	n, ok := new(big.Int).SetString(oracleText(raw), 10)
	if !ok {
		panic("bad 128-bit integer")
	}
	if n.Sign() < 0 {
		n.Add(n, new(big.Int).Lsh(big.NewInt(1), 128))
	}
	lo := new(big.Int).And(n, new(big.Int).SetUint64(math.MaxUint64)).Uint64()
	hi := new(big.Int).Rsh(n, 64).Uint64()
	return rs.FmtTestInt128(hi, lo, signed)
}

func oracleDuration(secs, nanos json.RawMessage) time.Duration {
	return time.Duration(oracleUnsigned(secs, 64))*time.Second + time.Duration(oracleInt(nanos))
}

func oracleStrings(raw json.RawMessage) []string {
	var items []string
	if err := json.Unmarshal(raw, &items); err != nil {
		panic(err)
	}
	if items == nil {
		items = []string{}
	}
	return items
}

// oracleValue builds the Go value a request describes: the counterpart of
// `value` in go/port/oracle/src/std_fmt.rs.
func oracleValue(raw json.RawMessage) any {
	parts := oracleParts(raw)
	tag := oracleText(parts[0])
	switch tag {
	case "i8":
		return int8(oracleSigned(parts[1], 8))
	case "i16":
		return int16(oracleSigned(parts[1], 16))
	case "i32":
		return int32(oracleSigned(parts[1], 32))
	case "i64":
		return oracleSigned(parts[1], 64)
	case "i128":
		return oracleWide(parts[1], true)
	case "isize":
		return int(oracleSigned(parts[1], 64))
	case "u8":
		return uint8(oracleUnsigned(parts[1], 8))
	case "u16":
		return uint16(oracleUnsigned(parts[1], 16))
	case "u32":
		return uint32(oracleUnsigned(parts[1], 32))
	case "u64":
		return oracleUnsigned(parts[1], 64)
	case "u128":
		return oracleWide(parts[1], false)
	case "usize":
		return uint(oracleUnsigned(parts[1], 64))
	case "f32":
		return math.Float32frombits(uint32(oracleUnsigned(parts[1], 32)))
	case "f64":
		return math.Float64frombits(oracleUnsigned(parts[1], 64))
	case "bool":
		var b bool
		if err := json.Unmarshal(parts[1], &b); err != nil {
			panic(err)
		}
		return b
	case "char":
		return rs.Char(rune(oracleInt(parts[1])))
	case "str":
		return oracleText(parts[1])
	case "unit":
		return rs.Unit{}
	case "dur":
		return oracleDuration(parts[1], parts[2])
	case "some":
		return rs.Some[any](oracleValue(parts[1]))
	case "none":
		return rs.None[any]()
	case "ok":
		return rs.Ok[any](oracleValue(parts[1]))
	case "err":
		return rs.Err[any](&oracleParseFailure{Line: uint32(oracleInt(parts[1])), Reason: oracleText(parts[2])})
	case "vec":
		items := []any{}
		for _, item := range oracleParts(parts[1]) {
			items = append(items, oracleValue(item))
		}
		return items
	case "deque":
		seq := &oracleSeq{items: []any{}}
		for _, item := range oracleParts(parts[1]) {
			seq.items = append(seq.items, oracleValue(item))
		}
		return seq
	case "bytes":
		bytes := []byte{}
		for _, item := range oracleParts(parts[1]) {
			bytes = append(bytes, byte(oracleInt(item)))
		}
		return bytes
	case "vec_i32":
		numbers := []int32{}
		for _, item := range oracleParts(parts[1]) {
			numbers = append(numbers, int32(oracleInt(item)))
		}
		return numbers
	case "vec_str":
		return oracleStrings(parts[1])
	case "arr3":
		items := oracleParts(parts[1])
		return [3]int32{int32(oracleInt(items[0])), int32(oracleInt(items[1])), int32(oracleInt(items[2]))}
	case "t2":
		return rs.T2(oracleValue(parts[1]), oracleValue(parts[2]))
	case "t3":
		return rs.T3(oracleValue(parts[1]), oracleValue(parts[2]), oracleValue(parts[3]))
	case "t4":
		return rs.T4(oracleValue(parts[1]), oracleValue(parts[2]), oracleValue(parts[3]), oracleValue(parts[4]))
	case "t5":
		return rs.Tuple5[any, any, any, any, any]{
			F0: oracleValue(parts[1]), F1: oracleValue(parts[2]), F2: oracleValue(parts[3]),
			F3: oracleValue(parts[4]), F4: oracleValue(parts[5]),
		}
	case "map_str":
		// A BTreeMap<String, _>: a Go map, or a collection that iterates in
		// key order, or one that does not and has to be sorted.
		entries := oracleParts(parts[1])
		switch oracleVariation(raw, 3) {
		case 0:
			m := map[string]any{}
			for _, entry := range entries {
				pair := oracleParts(entry)
				m[oracleText(pair[0])] = oracleValue(pair[1])
			}
			return m
		case 1:
			m := &oracleMap{sorted: true}
			keys := map[string]any{}
			for _, entry := range entries {
				pair := oracleParts(entry)
				keys[oracleText(pair[0])] = oracleValue(pair[1])
			}
			for _, key := range oracleSortedKeys(keys) {
				m.ReflectInsert(key, keys[key])
			}
			return m
		default:
			m := &oracleMap{sorted: false}
			seen := map[string]int{}
			for _, entry := range entries {
				pair := oracleParts(entry)
				key := oracleText(pair[0])
				if at, ok := seen[key]; ok {
					m.values[at] = oracleValue(pair[1])
					continue
				}
				seen[key] = len(m.keys)
				m.ReflectInsert(key, oracleValue(pair[1]))
			}
			return m
		}
	case "map_i64":
		m := map[int64]any{}
		for _, entry := range oracleParts(parts[1]) {
			pair := oracleParts(entry)
			m[oracleSigned(pair[0], 64)] = oracleValue(pair[1])
		}
		return m
	case "map_str_f64":
		return oracleScores(parts[1])
	case "set_str":
		seen := map[string]bool{}
		seq := &oracleSeq{items: []any{}, set: true, sorted: false}
		for _, item := range oracleStrings(parts[1]) {
			if !seen[item] {
				seen[item] = true
				seq.items = append(seq.items, item)
			}
		}
		return seq
	case "set_i64":
		seen := map[int64]bool{}
		seq := &oracleSeq{items: []any{}, set: true, sorted: false}
		for _, item := range oracleParts(parts[1]) {
			n := oracleSigned(item, 64)
			if !seen[n] {
				seen[n] = true
				seq.items = append(seq.items, n)
			}
		}
		return seq
	case "box":
		// A Box<T> is a pointer.
		inner := oracleValue(parts[1])
		return &inner
	case "UnitStruct":
		return oracleUnitStruct{}
	case "Empty":
		return oracleEmpty{}
	case "Point":
		point := oraclePoint{X: int32(oracleInt(parts[1])), Y: int32(oracleInt(parts[2]))}
		if oracleVariation(raw, 2) == 0 {
			return point
		}
		return &point
	case "Pair":
		return &oraclePair{F0: oracleValue(parts[1]), F1: oracleValue(parts[2])}
	case "Wrap":
		return oracleWrap{F0: oracleValue(parts[1])}
	case "SessionId":
		return oracleSessionId(oracleText(parts[1]))
	case "Meters":
		return oracleMeters(math.Float64frombits(oracleUnsigned(parts[1], 64)))
	case "Count":
		return oracleCount(oracleInt(parts[1]))
	case "Tags":
		return oracleTags(oracleStrings(parts[1]))
	case "Kind":
		return oracleKind(oracleInt(parts[1]))
	case "State":
		return oracleStateValue(parts[1])
	case "Record":
		return oracleRecordValue(parts[1])
	}
	panic("no oracle value tag " + tag)
}

func oracleSortedKeys(m map[string]any) []string {
	keys := make([]string, 0, len(m))
	for key := range m {
		keys = append(keys, key)
	}
	for i := 1; i < len(keys); i++ {
		for j := i; j > 0 && keys[j] < keys[j-1]; j-- {
			keys[j], keys[j-1] = keys[j-1], keys[j]
		}
	}
	return keys
}

func oracleScores(raw json.RawMessage) map[string]float64 {
	m := map[string]float64{}
	for _, entry := range oracleParts(raw) {
		pair := oracleParts(entry)
		m[oracleText(pair[0])] = math.Float64frombits(oracleUnsigned(pair[1], 64))
	}
	return m
}

func oracleStateValue(raw json.RawMessage) oracleState {
	parts := oracleParts(raw)
	switch variant := oracleText(parts[0]); variant {
	case "Idle":
		return &oracleState_Idle{}
	case "Running":
		return &oracleState_Running{Pid: uint32(oracleInt(parts[1])), Since: oracleDuration(parts[2], parts[3])}
	case "Failed":
		return &oracleState_Failed{F0: oracleText(parts[1]), F1: int32(oracleInt(parts[2]))}
	case "Wrapped":
		return &oracleState_Wrapped{F0: oracleStateValue(parts[1])}
	case "Maybe":
		if string(parts[1]) == "null" {
			return &oracleState_Maybe{F0: rs.None[uint8]()}
		}
		return &oracleState_Maybe{F0: rs.Some(uint8(oracleInt(parts[1])))}
	case "Payload":
		return &oracleState_Payload{F0: oracleValue(parts[1])}
	default:
		panic("no State variant " + variant)
	}
}

func oracleRecordValue(raw json.RawMessage) *oracleRecord {
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(raw, &fields); err != nil {
		panic(err)
	}
	record := &oracleRecord{
		Id:      oracleUnsigned(fields["id"], 64),
		Name:    oracleText(fields["name"]),
		Tags:    oracleStrings(fields["tags"]),
		Parent:  rs.None[*oracleRecord](),
		Scores:  oracleScores(fields["scores"]),
		Kind:    oracleKind(oracleInt(fields["kind"])),
		State:   oracleStateValue(fields["state"]),
		Raw:     []byte{},
		Elapsed: oracleDuration(oracleParts(fields["elapsed"])[0], oracleParts(fields["elapsed"])[1]),
		Pair:    rs.T2(int32(oracleInt(oracleParts(fields["pair"])[0])), oracleText(oracleParts(fields["pair"])[1])),
		Ch:      rs.Char(rune(oracleInt(fields["ch"]))),
		Unit:    rs.Unit{},
		Flag:    string(fields["flag"]) == "true",
		Ratio:   math.Float32frombits(uint32(oracleUnsigned(fields["ratio"], 32))),
		Session: oracleSessionId(oracleText(fields["session"])),
		Any:     oracleValue(fields["any"]),
	}
	if string(fields["parent"]) != "null" {
		record.Parent = rs.Some(oracleRecordValue(fields["parent"]))
	}
	for _, item := range oracleParts(fields["raw"]) {
		record.Raw = append(record.Raw, byte(oracleInt(item)))
	}
	if string(fields["private_note"]) == "null" {
		record.privateNote = rs.None[string]()
	} else {
		record.privateNote = rs.Some(oracleText(fields["private_note"]))
	}
	return record
}
