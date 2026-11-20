package qrcode

import (
	"encoding/hex"
	"encoding/json"
	"math/rand/v2"
	"os"
	"strconv"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
)

// The oracle (go/port/oracle/src/qrcode_oracle.rs) is a release build of the
// Rust crate: its arithmetic wraps and its debug assertions are off. The
// comparisons switch them off here too; the ported tests of the crate run
// with them on, as `cargo test` does.
func withoutDebugAssertions(t *testing.T) {
	saved := debugAssertions
	debugAssertions = false
	t.Cleanup(func() { debugAssertions = saved })
}

// liveOracle reports whether the comparison runs against Rust now
// (MJ_ORACLE=live). The generators add their largest cases only then, which
// keeps the recorded corpora small.
func liveOracle() bool {
	return os.Getenv("MJ_ORACLE") == "live"
}

// ---------------------------------------------------------------------------
// The data generator both sides expand: `generate` in the oracle module.

func oracleNext(state *uint64) uint64 {
	*state = *state*6364136223846793005 + 1442695040888963407
	return *state >> 33
}

const (
	oracleNumeric      = "0123456789"
	oracleAlphanumeric = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ $%*+-./:"
	oracleUrl          = "abcdefghijklmnopqrstuvwxyz0123456789-._~:/?#[]@!$&'()*+,;=%ABCXYZ"
)

func oracleGenerate(kind string, count int, seed uint64) []byte {
	state := seed ^ 0x9e3779b97f4a7c15
	out := make([]byte, 0, count)
	pick := func(alphabet string) byte {
		return alphabet[oracleNext(&state)%uint64(len(alphabet))]
	}
	for len(out) < count {
		switch kind {
		case "numeric":
			out = append(out, pick(oracleNumeric))
		case "alphanumeric":
			out = append(out, pick(oracleAlphanumeric))
		case "url":
			out = append(out, pick(oracleUrl))
		case "bytes":
			out = append(out, byte(oracleNext(&state)&0xff))
		case "kanji":
			lead := oracleNext(&state) % 43
			if lead < 31 {
				lead = 0x81 + lead
			} else {
				lead = 0xe0 + (lead - 31)
			}
			trail := 0x40 + oracleNext(&state)%0xbd
			if trail == 0x7f {
				trail = 0x80
			}
			out = append(out, byte(lead), byte(trail))
		case "mixed":
			kinds := []string{"numeric", "alphanumeric", "url", "bytes", "kanji"}
			sub := kinds[oracleNext(&state)%uint64(len(kinds))]
			run := 1 + int(oracleNext(&state)%24)
			subSeed := oracleNext(&state)
			out = append(out, oracleGenerate(sub, run, subSeed)...)
		default:
			panic("unknown data kind " + kind)
		}
	}
	return out[:count]
}

type oracleGen struct {
	Kind string `json:"kind"`
	Len  int    `json:"len"`
	Seed uint64 `json:"seed"`
}

type oracleData struct {
	Gen     *oracleGen `json:"gen"`
	DataHex string     `json:"data_hex"`
}

func (d oracleData) bytes() []byte {
	if d.Gen != nil {
		return oracleGenerate(d.Gen.Kind, d.Gen.Len, d.Gen.Seed)
	}
	data, err := hex.DecodeString(d.DataHex)
	if err != nil {
		panic(err)
	}
	return data
}

type oracleVersion struct {
	Normal *int16 `json:"normal"`
	Micro  *int16 `json:"micro"`
}

func (v *oracleVersion) version() Version {
	switch {
	case v == nil:
		return nil
	case v.Normal != nil:
		return &Version_Normal{F0: *v.Normal}
	case v.Micro != nil:
		return &Version_Micro{F0: *v.Micro}
	}
	return nil
}

func oracleLevel(name *string) rs.Option[EcLevel] {
	if name == nil {
		return rs.None[EcLevel]()
	}
	switch *name {
	case "L":
		return rs.Some(EcLevel_L)
	case "M":
		return rs.Some(EcLevel_M)
	case "Q":
		return rs.Some(EcLevel_Q)
	case "H":
		return rs.Some(EcLevel_H)
	}
	panic("unknown level " + *name)
}

func oraclePattern(name string) CanvasMaskPattern {
	for _, pattern := range canvasAllPatternsQr {
		if rs.DebugString(pattern) == name {
			return pattern
		}
	}
	panic("unknown mask pattern " + name)
}

type oracleCall struct {
	oracleData
	Push  string  `json:"push"`
	Value uint64  `json:"value"`
	Level *string `json:"level"`
}

type oracleRequest struct {
	oracleData
	Op      string         `json:"op"`
	Level   *string        `json:"level"`
	Version *oracleVersion `json:"version"`
	Full    bool           `json:"full"`
	Calls   []oracleCall   `json:"calls"`
	Size    int            `json:"size"`
	Pattern string         `json:"pattern"`
	Ec      oracleData     `json:"ec"`
}

// ---------------------------------------------------------------------------
// The Go side of the replies.

func oracleFnv(values []byte) string {
	digest := uint64(0xcbf29ce484222325)
	for _, value := range values {
		digest ^= uint64(value)
		digest *= 0x100000001b3
	}
	hexText := strconv.FormatUint(digest, 16)
	return strings.Repeat("0", 16-len(hexText)) + hexText
}

func oracleColorBytes(colors []Color) []byte {
	out := make([]byte, len(colors))
	for i, color := range colors {
		out[i] = byte(color)
	}
	return out
}

func oracleRows(colors []Color, width int) []string {
	var rows []string
	for start := 0; start < len(colors); start += width {
		var row strings.Builder
		for _, color := range colors[start : start+width] {
			row.WriteByte(ColorSelect[byte](color, '#', '.'))
		}
		rows = append(rows, row.String())
	}
	return rows
}

func oracleErrorValue(err error) any {
	return map[string]any{
		"err":     rs.DebugString(err),
		"display": err.Error(),
		"padded":  rs.Format("{error:>30}|{error:<4.2}", rs.Arg("error", err)),
		"context": rs.Format("encode web login QR: {error}", rs.Arg("error", err)),
	}
}

func oracleCodeValue(code *QrCode, full bool) any {
	colors := code.ToColors()
	width := code.Width()
	functional := make([]byte, 0, width*width)
	for y := range width {
		for x := range width {
			functional = append(functional, rs.FromBool[byte](code.IsFunctional(x, y)))
		}
	}
	render := QrCodeRender[rune](code).Build()
	renderStr := QrCodeRender[string](code).DarkColor("##").LightColor("  ").ModuleDimensions(2, 1).Build()
	value := map[string]any{
		"version":            rs.DebugString(code.Version()),
		"level":              rs.DebugString(code.ErrorCorrectionLevel()),
		"width":              width,
		"max_allowed_errors": code.MaxAllowedErrors(),
		"modules":            oracleFnv(oracleColorBytes(colors)),
		"functional":         oracleFnv(functional),
		"render":             oracleFnv([]byte(render)),
		"render_str":         oracleFnv([]byte(renderStr)),
		"corner":             code.Index(width-1, width-1) == Color_Dark,
	}
	if full {
		value["rows"] = oracleRows(colors, width)
	}
	return value
}

func oracleResult(err error) any {
	if err != nil {
		return rs.DebugString(err)
	}
	return "ok"
}

func oracleSegments(segments []OptimizeSegment) []any {
	out := []any{}
	for _, s := range segments {
		out = append(out, []any{rs.DebugString(s.Mode), s.Begin, s.End})
	}
	return out
}

// answer is the Go side of every request.
func answer(raw json.RawMessage) any {
	var request oracleRequest
	if err := json.Unmarshal(raw, &request); err != nil {
		panic(err)
	}
	return oracle.Catch(func() any {
		switch request.Op {
		case "encode":
			data := request.bytes()
			level := oracleLevel(request.Level)
			version := request.Version.version()
			var code *QrCode
			var err error
			switch {
			case version != nil:
				code, err = QrCodeWithVersion(data, version, level.Unwrap())
			case level.IsSome():
				code, err = QrCodeWithErrorCorrectionLevel(data, level.Unwrap())
			default:
				code, err = QrCodeNew(data)
			}
			if err != nil {
				return oracleErrorValue(err)
			}
			return map[string]any{"ok": oracleCodeValue(code, request.Full)}
		case "bits":
			bits := BitsBitsNew(request.Version.version())
			results := []any{}
			for _, call := range request.Calls {
				var err error
				switch call.Push {
				case "numeric":
					err = bits.PushNumericData(call.bytes())
				case "alphanumeric":
					err = bits.PushAlphanumericData(call.bytes())
				case "byte":
					err = bits.PushByteData(call.bytes())
				case "kanji":
					err = bits.PushKanjiData(call.bytes())
				case "optimal":
					err = bits.PushOptimalData(call.bytes())
				case "eci":
					err = bits.PushEciDesignator(uint32(call.Value))
				case "fnc1_first":
					err = bits.PushFnc1FirstPosition()
				case "fnc1_second":
					err = bits.PushFnc1SecondPosition(uint8(call.Value))
				case "structured_append":
					err = bits.PushModeIndicator(&BitsExtendedMode_StructuredAppend{})
				case "terminator":
					err = bits.PushTerminator(oracleLevel(call.Level).Unwrap())
				default:
					panic("unknown push " + call.Push)
				}
				results = append(results, oracleResult(err))
				results = append(results, map[string]any{"len": bits.Len(), "empty": bits.IsEmpty()})
			}
			maxLen := []any{}
			for _, level := range []EcLevel{EcLevel_L, EcLevel_M, EcLevel_Q, EcLevel_H} {
				if n, err := bits.MaxLen(level); err != nil {
					maxLen = append(maxLen, rs.DebugString(err))
				} else {
					maxLen = append(maxLen, n)
				}
			}
			return map[string]any{"results": results, "max_len": maxLen, "bytes": hex.EncodeToString(bits.IntoBytes())}
		case "segments":
			data := request.bytes()
			version := request.Version.version()
			parsed := OptimizeParserNew(data).Collect()
			optimized := OptimizeOptimizerNew(&optimizeSliceIter{segments: parsed}, version).Collect()
			lens := []int{}
			for _, s := range parsed {
				lens = append(lens, s.EncodedLen(version))
			}
			return map[string]any{
				"parsed":      oracleSegments(parsed),
				"optimized":   oracleSegments(optimized),
				"parsed_lens": lens,
				"total":       OptimizeTotalEncodedLen(optimized, version),
			}
		case "encode_auto":
			bits, err := BitsEncodeAuto(request.bytes(), oracleLevel(request.Level).Unwrap())
			if err != nil {
				return map[string]any{"err": rs.DebugString(err)}
			}
			version := rs.DebugString(bits.Version())
			length := bits.Len()
			return map[string]any{"version": version, "len": length, "bytes": oracleFnv(bits.IntoBytes())}
		case "codewords":
			blocks, ec, err := EcConstructCodewords(request.bytes(), request.Version.version(), oracleLevel(request.Level).Unwrap())
			if err != nil {
				return map[string]any{"err": rs.DebugString(err)}
			}
			return map[string]any{
				"data":      oracleFnv(blocks),
				"data_head": hex.EncodeToString(blocks[:min(len(blocks), 12)]),
				"data_len":  len(blocks),
				"ec":        oracleFnv(ec),
				"ec_head":   hex.EncodeToString(ec[:min(len(ec), 12)]),
				"ec_len":    len(ec),
			}
		case "ecc":
			return map[string]any{"ec": hex.EncodeToString(EcCreateErrorCorrectionCode(request.bytes(), request.Size))}
		case "max_allowed_errors":
			errors, err := EcMaxAllowedErrors(request.Version.version(), oracleLevel(request.Level).Unwrap())
			if err != nil {
				return map[string]any{"err": rs.DebugString(err)}
			}
			return map[string]any{"ok": errors}
		case "mask":
			version := request.Version.version()
			canvas := CanvasCanvasNew(version, oracleLevel(request.Level).Unwrap())
			canvas.DrawAllFunctionalPatterns()
			canvas.DrawData(request.bytes(), request.Ec.bytes())
			bools := canvas.ToBools()
			best := canvas.ApplyBestMask().IntoColors()
			canvas.ApplyMask(oraclePattern(request.Pattern))
			colors := canvas.IntoColors()
			unmasked := make([]byte, len(bools))
			for i, b := range bools {
				unmasked[i] = rs.FromBool[byte](b)
			}
			value := map[string]any{
				"unmasked": oracleFnv(unmasked),
				"masked":   oracleFnv(oracleColorBytes(colors)),
				"best":     oracleFnv(oracleColorBytes(best)),
			}
			if request.Full {
				value["rows"] = oracleRows(colors, int(version.Width()))
			}
			return value
		default:
			panic("unknown op " + request.Op)
		}
	})
}

// ---------------------------------------------------------------------------
// Generators. All of them are deterministic: fixed lists, then choices made
// by a seeded generator.

type generator struct {
	random *rand.Rand
	seed   uint64
}

func newGenerator(seed uint64) *generator {
	return &generator{random: rand.New(rand.NewPCG(seed, 0x71726f6465))}
}

func (g *generator) pick(options []string) string {
	return options[g.random.IntN(len(options))]
}

func (g *generator) chance(percent int) bool {
	return g.random.IntN(100) < percent
}

// gen is a data spec for the oracle's generator, with a fresh seed.
func (g *generator) gen(kind string, length int) map[string]any {
	g.seed++
	return map[string]any{"kind": kind, "len": length, "seed": g.seed*7919 + 13}
}

var genKinds = []string{"numeric", "alphanumeric", "url", "bytes", "kanji", "mixed"}

var genLevels = []string{"L", "M", "Q", "H"}

func normal(n int) map[string]any { return map[string]any{"normal": n} }

func micro(n int) map[string]any { return map[string]any{"micro": n} }

// allVersions is every valid version: Normal(1) to Normal(40), Micro(1) to
// Micro(4).
func allVersions() []map[string]any {
	var out []map[string]any
	for n := 1; n <= 40; n++ {
		out = append(out, normal(n))
	}
	for n := 1; n <= 4; n++ {
		out = append(out, micro(n))
	}
	return out
}

func versionOf(spec map[string]any) Version {
	if n, ok := spec["normal"]; ok {
		return &Version_Normal{F0: int16(n.(int))}
	}
	return &Version_Micro{F0: int16(spec["micro"].(int))}
}

// capacityChars estimates how many characters of a kind fit in a version at
// a level, so lengths can be aimed at the boundary where DataTooLong begins.
func capacityChars(kind string, version Version, level EcLevel) int {
	bits, err := VersionFetch(version, level, bitsDataLengths[:])
	if err != nil {
		return 4
	}
	bits -= 20
	switch kind {
	case "numeric":
		return max(bits*3/10, 1)
	case "alphanumeric":
		return max(bits*2/11, 1)
	case "kanji":
		return max(bits/13*2, 2)
	default:
		return max(bits/8, 1)
	}
}

// webLoginUrls are inputs like the one Mjolnir draws: the web viewer's login
// address.
var webLoginUrls = []string{
	"http://127.0.0.1:6767/",
	"http://127.0.0.1:6767/login?token=0123456789abcdef0123456789abcdef",
	"http://192.168.1.20:6767/login?token=Zm9vYmFyYmF6cXV4cXV1eA",
	"https://mjolnir.example.com/web/login#token=a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0",
	"http://[::1]:6767/login?token=00000000-0000-0000-0000-000000000000",
	"HTTP://EXAMPLE.COM/LOGIN?TOKEN=ABC123",
	"",
	"01234567",
	"HELLO WORLD",
	"This is a mixed data test. 1234567890",
	"日本語のテキスト UTF-8 \U0001F600",
}

// ---------------------------------------------------------------------------
// The comparisons.

// TestDiffAgainstRust_encode compares whole QR codes: the version and level
// chosen, the module matrix, the functional modules, the renderings, and the
// errors with their texts, at every level and in every version.
func TestDiffAgainstRust_encode(t *testing.T) {
	withoutDebugAssertions(t)
	oracle.Check(t, "qrcode", "qrcode_encode", func(yield func(any)) {
		for _, input := range webLoginUrls {
			for _, level := range genLevels {
				yield(map[string]any{"op": "encode", "data_hex": hex.EncodeToString([]byte(input)), "level": level, "full": true})
			}
			yield(map[string]any{"op": "encode", "data_hex": hex.EncodeToString([]byte(input))})
		}
		// Micro QR codes whose data ends inside the last byte: the padding
		// count goes below zero (see Bits.PushTerminator).
		for _, digits := range []int{18, 19, 20, 21, 22, 23} {
			yield(map[string]any{"op": "encode", "data_hex": hex.EncodeToString([]byte(strings.Repeat("7", digits))), "level": "L", "version": micro(3)})
		}
		for _, digits := range []int{14, 15, 16, 17, 18} {
			yield(map[string]any{"op": "encode", "data_hex": hex.EncodeToString([]byte(strings.Repeat("7", digits))), "level": "M", "version": micro(3)})
		}
		g := newGenerator(1)
		// Automatic version selection, lengths from empty to past the
		// largest code.
		for _, kind := range genKinds {
			for _, level := range append(genLevels, "") {
				lengths := []int{0, 1, 2, 3, 5, 8, 13, 17, 25, 34, 50, 77, 100, 150}
				for range 12 {
					lengths = append(lengths, g.random.IntN(800))
				}
				if liveOracle() {
					for range 8 {
						lengths = append(lengths, 800+g.random.IntN(7400))
					}
				}
				for _, length := range lengths {
					request := map[string]any{"op": "encode", "gen": g.gen(kind, length)}
					if level != "" {
						request["level"] = level
					}
					yield(request)
				}
			}
		}
		// The largest codes and the first lengths that do not fit, at each
		// level; recorded for the cheapest kind only.
		boundaryKinds := []string{"numeric"}
		if liveOracle() {
			boundaryKinds = genKinds
		}
		for _, kind := range boundaryKinds {
			for _, level := range genLevels {
				capacity := capacityChars(kind, &Version_Normal{F0: 40}, oracleLevel(&level).Unwrap())
				for _, delta := range []int{-40, -1, 0, 1, 2, 20, 60} {
					yield(map[string]any{"op": "encode", "gen": g.gen(kind, max(capacity+delta, 0)), "level": level})
				}
			}
		}
		// A chosen version, around the boundary of what fits in it.
		for _, version := range allVersions() {
			for _, level := range genLevels {
				capacity := capacityChars("bytes", versionOf(version), oracleLevel(&level).Unwrap())
				for range 3 {
					kind := g.pick(genKinds)
					length := capacity + g.random.IntN(capacity/3+6) - capacity/6 - 3
					if !liveOracle() && versionOf(version).Width() > 60 {
						length = min(length, 40+g.random.IntN(60))
					}
					yield(map[string]any{"op": "encode", "gen": g.gen(kind, max(length, 0)), "level": level, "version": version})
				}
			}
		}
		for range 600 {
			request := map[string]any{"op": "encode", "gen": g.gen(g.pick(genKinds), g.random.IntN(300))}
			request["level"] = g.pick(genLevels)
			if g.chance(30) {
				request["version"] = map[string]any{"normal": 1 + g.random.IntN(12)}
			} else if g.chance(10) {
				request["version"] = micro(1 + g.random.IntN(4))
			}
			yield(request)
		}
	}, answer)
}

// TestDiffAgainstRust_bits compares sequences of pushes onto one Bits: each
// answer, the length after it, and the bytes at the end.
func TestDiffAgainstRust_bits(t *testing.T) {
	withoutDebugAssertions(t)
	oracle.Check(t, "qrcode", "qrcode_bits", func(yield func(any)) {
		g := newGenerator(2)
		versions := allVersions()
		versions = append(versions, normal(0), normal(41), micro(5))
		pushes := []string{"numeric", "alphanumeric", "byte", "kanji", "optimal", "eci", "fnc1_first", "fnc1_second", "structured_append", "terminator"}
		for range 1500 {
			version := versions[g.random.IntN(len(versions))]
			calls := []any{}
			for range 1 + g.random.IntN(5) {
				push := g.pick(pushes)
				call := map[string]any{"push": push}
				switch push {
				case "numeric", "alphanumeric", "byte", "kanji", "optimal":
					// Mostly data of the push's own kind; sometimes not, which
					// the crate does not check.
					kind := map[string]string{"numeric": "numeric", "alphanumeric": "alphanumeric", "byte": "bytes", "kanji": "kanji", "optimal": g.pick(genKinds)}[push]
					if g.chance(15) {
						kind = g.pick(genKinds)
					}
					call["gen"] = g.gen(kind, g.random.IntN(40))
				case "eci":
					values := []int{0, 9, 26, 127, 128, 899, 16383, 16384, 999999, 1000000, g.random.IntN(2000000)}
					call["value"] = values[g.random.IntN(len(values))]
				case "fnc1_second":
					call["value"] = g.random.IntN(256)
				case "terminator":
					call["level"] = g.pick(genLevels)
				}
				calls = append(calls, call)
			}
			if g.chance(70) {
				calls = append(calls, map[string]any{"push": "terminator", "level": g.pick(genLevels)})
			}
			yield(map[string]any{"op": "bits", "version": version, "calls": calls})
		}
	}, answer)
}

// TestDiffAgainstRust_segments compares the parser's segments and the
// optimizer's merges of them in each version.
func TestDiffAgainstRust_segments(t *testing.T) {
	withoutDebugAssertions(t)
	oracle.Check(t, "qrcode", "qrcode_segments", func(yield func(any)) {
		fixed := [][]byte{
			[]byte("01049123451234591597033130128%10ABC123"), []byte("\x82\xa0\x81\x41\x41\xb1\x81\xf0"),
			[]byte("\xe3\x81\x82\xe3\x80\x81A\xef\xbd\xb1\xe2\x84\xab"), []byte("\x81\x30"), []byte("\xeb\xc0"),
			[]byte("\x81\x7f"), []byte("\x81\x40\x81"), []byte("ABC123abcd"), {}, []byte("\xeb"), []byte("\xeb\x40\xeb"),
			[]byte("\x81"), []byte("\xe0\x40\x41"), []byte("1\x81\x40"),
		}
		for _, data := range fixed {
			for _, version := range []map[string]any{normal(1), normal(9), normal(10), normal(27), micro(1), micro(4)} {
				yield(map[string]any{"op": "segments", "data_hex": hex.EncodeToString(data), "version": version})
			}
		}
		g := newGenerator(3)
		versions := allVersions()
		for range 1500 {
			yield(map[string]any{"op": "segments", "gen": g.gen(g.pick(genKinds), g.random.IntN(120)), "version": versions[g.random.IntN(len(versions))]})
		}
		// Every byte, alone and before every class of byte.
		for b := range 256 {
			yield(map[string]any{"op": "segments", "data_hex": hex.EncodeToString([]byte{byte(b), 0x40, byte(b), 0x30, 0xeb, byte(b)}), "version": normal(1)})
		}
		for range 400 {
			yield(map[string]any{"op": "encode_auto", "gen": g.gen(g.pick(genKinds), g.random.IntN(1500)), "level": g.pick(genLevels)})
		}
	}, answer)
}

// TestDiffAgainstRust_codewords compares the Reed-Solomon codes and their
// interleaving in every version and level, and what happens when the data
// does not have the length the version expects.
func TestDiffAgainstRust_codewords(t *testing.T) {
	withoutDebugAssertions(t)
	oracle.Check(t, "qrcode", "qrcode_codewords", func(yield func(any)) {
		g := newGenerator(4)
		for _, version := range allVersions() {
			for _, level := range genLevels {
				perBlock, err := VersionFetch(versionOf(version), oracleLevel(&level).Unwrap(), ecDataBytesPerBlock[:])
				total := 20
				if err == nil {
					total = perBlock[0]*perBlock[1] + perBlock[2]*perBlock[3]
				}
				// The right length, and lengths around it.
				lengths := []int{total}
				if versionOf(version).Width() <= 45 || liveOracle() {
					lengths = append(lengths, total-1, total+1, total+g.random.IntN(30), g.random.IntN(total+1))
				}
				for _, length := range lengths {
					yield(map[string]any{"op": "codewords", "gen": g.gen("bytes", max(length, 0)), "version": version, "level": level})
				}
			}
		}
		for size := range 72 {
			yield(map[string]any{"op": "ecc", "gen": g.gen("bytes", g.random.IntN(40)), "size": size})
		}
		for _, version := range append(allVersions(), normal(0), normal(41), micro(0), micro(5)) {
			for _, level := range genLevels {
				yield(map[string]any{"op": "max_allowed_errors", "version": version, "level": level})
			}
		}
	}, answer)
}

// TestDiffAgainstRust_mask compares the canvas after data placement and each
// mask, and the mask the penalty scores choose, in every version.
func TestDiffAgainstRust_mask(t *testing.T) {
	withoutDebugAssertions(t)
	oracle.Check(t, "qrcode", "qrcode_mask", func(yield func(any)) {
		g := newGenerator(5)
		patterns := make([]string, 0, len(canvasAllPatternsQr))
		for _, pattern := range canvasAllPatternsQr {
			patterns = append(patterns, rs.DebugString(pattern))
		}
		for _, version := range allVersions() {
			v := versionOf(version)
			for _, level := range genLevels {
				perBlock, err := VersionFetch(v, oracleLevel(&level).Unwrap(), ecDataBytesPerBlock[:])
				if err != nil {
					continue
				}
				ecBytes, _ := VersionFetch(v, oracleLevel(&level).Unwrap(), ecEcBytesPerBlock[:])
				dataLen := perBlock[0]*perBlock[1] + perBlock[2]*perBlock[3]
				ecLen := ecBytes * (perBlock[1] + perBlock[3])
				count := 2
				if v.Width() <= 57 || liveOracle() {
					count = 4
				}
				for i := range count {
					request := map[string]any{
						"op":      "mask",
						"gen":     g.gen("bytes", dataLen),
						"ec":      map[string]any{"gen": g.gen("bytes", ecLen)},
						"version": version,
						"level":   level,
						"pattern": g.pick(patterns),
						"full":    v.Width() <= 25 && i == 0,
					}
					if i == count-1 {
						// Too little or too much data for the canvas.
						request["gen"] = g.gen("bytes", g.random.IntN(dataLen+20))
					}
					yield(request)
				}
			}
		}
	}, answer)
}
