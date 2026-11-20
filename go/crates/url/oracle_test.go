package url

import (
	"encoding/hex"
	"encoding/json"
	"fmt"
	"math/rand/v2"
	"net/netip"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
)

// The oracle is a release build of the Rust crate, in which debug assertions
// are off. The comparisons below switch them off here too; the ported tests
// of the crate run with them on, as `cargo test` does.
func withoutDebugAssertions(t *testing.T) {
	saved := debugAssertions
	debugAssertions = false
	t.Cleanup(func() { debugAssertions = saved })
}

// ---------------------------------------------------------------------------
// The Go side of the oracle's replies (go/port/oracle/src/url_oracle.rs).

func hostValue(host Host) any {
	switch h := host.(type) {
	case *Host_Domain:
		return map[string]any{"Domain": h.F0}
	case *Host_Ipv4:
		octets := h.F0.As4()
		return map[string]any{"Ipv4": bytesAsNumbers(octets[:])}
	case *Host_Ipv6:
		octets := h.F0.As16()
		return map[string]any{"Ipv6": bytesAsNumbers(octets[:])}
	default:
		panic(rs.Unreachable(host))
	}
}

func bytesAsNumbers(b []byte) []int {
	out := make([]int, len(b))
	for i, v := range b {
		out[i] = int(v)
	}
	return out
}

func optionValue[T any](o rs.Option[T]) any {
	if v, ok := o.Get(); ok {
		return v
	}
	return nil
}

func optionHostValue(o rs.Option[Host]) any {
	if v, ok := o.Get(); ok {
		return hostValue(v)
	}
	return nil
}

func errorValue(err error) any {
	parseError := err.(ParseError)
	return map[string]any{"err": parseError.rustName(), "display": parseError.Error()}
}

var allPositions = [16]Position{
	Position_BeforeScheme,
	Position_AfterScheme,
	Position_BeforeUsername,
	Position_AfterUsername,
	Position_BeforePassword,
	Position_AfterPassword,
	Position_BeforeHost,
	Position_AfterHost,
	Position_BeforePort,
	Position_AfterPort,
	Position_BeforePath,
	Position_AfterPath,
	Position_BeforeQuery,
	Position_AfterQuery,
	Position_BeforeFragment,
	Position_AfterFragment,
}

func pairsValue(pairs []rs.Tuple2[string, string]) [][]string {
	out := [][]string{}
	for _, pair := range pairs {
		out = append(out, []string{pair.F0, pair.F1})
	}
	return out
}

// dumpUrl is the oracle's `dump`. It reads the URL in the same order, so
// that a URL whose accessors panic panics in the same one.
func dumpUrl(url *Url) any {
	origin := url.Origin()
	var originValue any
	switch o := origin.(type) {
	case *Origin_Opaque:
		originValue = map[string]any{"opaque": true}
	case *Origin_Tuple:
		originValue = map[string]any{"scheme": o.F0, "host": hostValue(o.F1), "port": o.F2}
	}
	hasPassword := url.Password().IsSome()
	hasUsername := url.Username() != ""
	positions := make([]any, 0, len(allPositions))
	for _, position := range allPositions {
		if (position == Position_BeforePassword || position == Position_AfterPassword) && hasUsername && !hasPassword {
			positions = append(positions, nil)
		} else {
			positions = append(positions, len(url.IndexRangeTo(position)))
		}
	}
	out := map[string]any{}
	out["href"] = url.AsStr()
	out["to_string"] = url.String()
	out["into_string"] = StringFromUrl(*url)
	out["scheme"] = url.Scheme()
	out["is_special"] = url.IsSpecial()
	out["has_authority"] = url.HasAuthority()
	out["authority"] = url.Authority()
	out["cannot_be_a_base"] = url.CannotBeABase()
	out["username"] = url.Username()
	out["password"] = optionValue(url.Password())
	out["has_host"] = url.HasHost()
	out["host_str"] = optionValue(url.HostStr())
	out["host"] = optionHostValue(url.Host())
	out["domain"] = optionValue(url.Domain())
	out["port"] = optionValue(url.Port())
	out["port_or_known_default"] = optionValue(url.PortOrKnownDefault())
	out["path"] = url.Path()
	out["path_segments"] = optionValue(url.PathSegments())
	out["query"] = optionValue(url.Query())
	out["query_pairs"] = pairsValue(url.QueryPairs().Collect())
	out["fragment"] = optionValue(url.Fragment())
	out["origin"] = originValue
	out["origin_is_tuple"] = origin.IsTuple()
	out["origin_ascii"] = origin.AsciiSerialization()
	out["origin_unicode"] = origin.UnicodeSerialization()
	out["positions"] = positions
	out["full"] = url.IndexRangeFull()
	quirks := map[string]any{}
	quirks["href"] = QuirksHref(url)
	quirks["origin"] = QuirksOrigin(url)
	quirks["protocol"] = QuirksProtocol(url)
	quirks["username"] = QuirksUsername(url)
	quirks["password"] = QuirksPassword(url)
	quirks["host"] = QuirksHost(url)
	quirks["hostname"] = QuirksHostname(url)
	quirks["port"] = QuirksPort(url)
	quirks["pathname"] = QuirksPathname(url)
	quirks["search"] = QuirksSearch(url)
	quirks["hash"] = QuirksHash(url)
	out["quirks"] = quirks
	if err := url.CheckInvariants(); err != nil {
		out["check_invariants"] = err.Error()
	} else {
		out["check_invariants"] = nil
	}
	return out
}

// briefUrl is the oracle's `brief`.
func briefUrl(url *Url) any {
	return []any{
		url.AsStr(),
		url.Scheme(),
		url.Username(),
		optionValue(url.Password()),
		optionValue(url.HostStr()),
		optionHostValue(url.Host()),
		optionValue(url.Port()),
		url.Path(),
		optionValue(url.Query()),
		optionValue(url.Fragment()),
	}
}

func parseResult(url Url, err error, brief bool) any {
	if err != nil {
		return errorValue(err)
	}
	if brief {
		return map[string]any{"ok": briefUrl(&url)}
	}
	return map[string]any{"ok": dumpUrl(&url)}
}

type oracleRequest struct {
	Op     string      `json:"op"`
	Input  string      `json:"input"`
	Base   *string     `json:"base"`
	Brief  bool        `json:"brief"`
	Href   string      `json:"href"`
	Steps  []setStep   `json:"steps"`
	Url    string      `json:"url"`
	Opaque bool        `json:"opaque"`
	Prefix string      `json:"prefix"`
	Suffix string      `json:"suffix"`
	Start  uint32      `json:"start"`
	End    uint32      `json:"end"`
	Hex    string      `json:"hex"`
	Target string      `json:"target"`
	Calls  []setCall   `json:"calls"`
	Params [][2]string `json:"params"`
}

type setStep struct {
	Set   string    `json:"set"`
	Value *string   `json:"value"`
	Port  *uint16   `json:"port"`
	Calls []setCall `json:"calls"`
}

type setCall struct {
	Call     string      `json:"call"`
	Segment  string      `json:"segment"`
	Segments []string    `json:"segments"`
	Name     string      `json:"name"`
	Value    string      `json:"value"`
	Pairs    [][2]string `json:"pairs"`
	Keys     []string    `json:"keys"`
}

func unitResult(err error) any {
	if err != nil {
		return "err"
	}
	return "ok"
}

func optionOfPtr[T any](p *T) rs.Option[T] {
	if p == nil {
		return rs.None[T]()
	}
	return rs.Some(*p)
}

func pairsOf(pairs [][2]string) []rs.Tuple2[string, string] {
	out := []rs.Tuple2[string, string]{}
	for _, pair := range pairs {
		out = append(out, rs.T2(pair[0], pair[1]))
	}
	return out
}

// applySetter is the oracle's `apply_setter`.
func applySetter(url *Url, step setStep) any {
	value := optionOfPtr(step.Value)
	text := value.UnwrapOr("")
	switch step.Set {
	case "fragment":
		url.SetFragment(value)
		return "ok"
	case "query":
		url.SetQuery(value)
		return "ok"
	case "path":
		url.SetPath(text)
		return "ok"
	case "port":
		return unitResult(url.SetPort(optionOfPtr(step.Port)))
	case "host":
		if err := url.SetHost(value); err != nil {
			return errorValue(err)
		}
		return "ok"
	case "ip_host":
		return unitResult(url.SetIpHost(netip.MustParseAddr(text)))
	case "password":
		return unitResult(url.SetPassword(value))
	case "username":
		return unitResult(url.SetUsername(text))
	case "scheme":
		return unitResult(url.SetScheme(text))
	case "quirks_href":
		if err := QuirksSetHref(url, text); err != nil {
			return errorValue(err)
		}
		return "ok"
	case "quirks_protocol":
		return unitResult(QuirksSetProtocol(url, text))
	case "quirks_username":
		return unitResult(QuirksSetUsername(url, text))
	case "quirks_password":
		return unitResult(QuirksSetPassword(url, text))
	case "quirks_host":
		return unitResult(QuirksSetHost(url, text))
	case "quirks_hostname":
		return unitResult(QuirksSetHostname(url, text))
	case "quirks_port":
		return unitResult(QuirksSetPort(url, text))
	case "quirks_pathname":
		QuirksSetPathname(url, text)
		return "ok"
	case "quirks_search":
		QuirksSetSearch(url, text)
		return "ok"
	case "quirks_hash":
		QuirksSetHash(url, text)
		return "ok"
	case "path_segments":
		segments, err := url.PathSegmentsMut()
		if err != nil {
			return "err"
		}
		defer segments.Drop()
		for _, call := range step.Calls {
			switch call.Call {
			case "clear":
				segments.Clear()
			case "pop":
				segments.Pop()
			case "pop_if_empty":
				segments.PopIfEmpty()
			case "push":
				segments.Push(call.Segment)
			case "extend":
				segments.Extend(call.Segments)
			default:
				panic("unknown path_segments call " + call.Call)
			}
		}
		return "ok"
	case "query_pairs":
		pairs := url.QueryPairsMut()
		defer pairs.Drop()
		for _, call := range step.Calls {
			switch call.Call {
			case "clear":
				pairs.Clear()
			case "append_pair":
				pairs.AppendPair(call.Name, call.Value)
			case "append_key_only":
				pairs.AppendKeyOnly(call.Name)
			case "extend_pairs":
				pairs.ExtendPairs(pairsOf(call.Pairs))
			case "extend_keys_only":
				pairs.ExtendKeysOnly(call.Keys)
			case "finish":
				pairs.Finish()
			default:
				panic("unknown query_pairs call " + call.Call)
			}
		}
		return "ok"
	default:
		panic("unknown setter " + step.Set)
	}
}

func hostText(input string) string {
	host, err := HostParse(input)
	if err != nil {
		return "-" + err.(ParseError).rustName()
	}
	return "+" + host.String()
}

func fnv(digest *uint64, bytes string) {
	for i := 0; i < len(bytes); i++ {
		*digest ^= uint64(bytes[i])
		*digest *= 0x100000001b3
	}
	*digest ^= 0xff
	*digest *= 0x100000001b3
}

func isScalarValue(code uint32) bool {
	return code <= 0x10FFFF && !(0xD800 <= code && code <= 0xDFFF)
}

// answer is the Go side of every request.
func answer(raw json.RawMessage) any {
	var request oracleRequest
	if err := json.Unmarshal(raw, &request); err != nil {
		panic(err)
	}
	return oracle.Catch(func() any {
		switch request.Op {
		case "parse":
			if request.Base != nil {
				base, err := UrlParse(*request.Base)
				if err != nil {
					return map[string]any{"base": errorValue(err)}
				}
				url, err := base.Join(request.Input)
				return parseResult(url, err, request.Brief)
			}
			url, err := UrlParse(request.Input)
			return parseResult(url, err, request.Brief)
		case "violations":
			options := UrlOptions()
			if request.Base != nil {
				base, err := UrlParse(*request.Base)
				if err != nil {
					return map[string]any{"base": errorValue(err)}
				}
				options = options.BaseUrl(rs.Some(&base))
			}
			seen := [][]string{}
			callback := func(violation SyntaxViolation) {
				seen = append(seen, []string{rs.VariantOf(violation).RustName, violation.String(), violation.Description()})
			}
			url, err := options.SyntaxViolationCallback(rs.Some(callback)).Parse(request.Input)
			var result any
			if err != nil {
				result = errorValue(err)
			} else {
				result = url.AsStr()
			}
			return map[string]any{"violations": seen, "result": result}
		case "encoding_override":
			encode := func(text string) []byte { return []byte(asciiUpper(text)) }
			url, err := UrlOptions().EncodingOverride(rs.Some(encode)).Parse(request.Input)
			return parseResult(url, err, request.Brief)
		case "parse_with_params":
			url, err := UrlParseWithParams(request.Input, pairsOf(request.Params))
			return parseResult(url, err, request.Brief)
		case "set":
			url, err := UrlParse(request.Href)
			if err != nil {
				return map[string]any{"href": errorValue(err)}
			}
			results := []any{}
			states := []string{}
			for _, step := range request.Steps {
				results = append(results, applySetter(&url, step))
				states = append(states, url.AsStr())
			}
			if request.Brief {
				return map[string]any{"results": results, "states": states, "final": briefUrl(&url)}
			}
			return map[string]any{"results": results, "states": states, "final": dumpUrl(&url)}
		case "make_relative":
			base, errBase := UrlParse(*request.Base)
			url, errUrl := UrlParse(request.Url)
			if errBase != nil || errUrl != nil {
				return map[string]any{"unparsed": true}
			}
			return map[string]any{"relative": optionValue(base.MakeRelative(&url))}
		case "host_parse":
			var host Host
			var err error
			if request.Opaque {
				host, err = HostParseOpaque(request.Input)
			} else {
				host, err = HostParse(request.Input)
			}
			if err != nil {
				return errorValue(err)
			}
			return map[string]any{"ok": hostValue(host), "display": host.String()}
		case "domain":
			return map[string]any{
				"ascii":   QuirksDomainToAscii(request.Input),
				"unicode": QuirksDomainToUnicode(request.Input),
				"host":    hostText(request.Input),
			}
		case "host_digest":
			digest := uint64(0xcbf29ce484222325)
			for code := request.Start; code < request.End; code++ {
				if !isScalarValue(code) {
					continue
				}
				fnv(&digest, hostText(request.Prefix+string(rune(code))+request.Suffix))
			}
			return map[string]any{"digest": fmt.Sprintf("%016x", digest)}
		case "host_list":
			answers := []string{}
			for code := request.Start; code < request.End; code++ {
				if !isScalarValue(code) {
					continue
				}
				answers = append(answers, hostText(request.Prefix+string(rune(code))+request.Suffix))
			}
			return map[string]any{"answers": answers}
		case "form_parse":
			input, _ := hex.DecodeString(request.Hex)
			return map[string]any{
				"pairs": pairsValue(FormUrlencodedParse(input).Collect()),
				"owned": pairsValue(FormUrlencodedParse(input).IntoOwned().Collect()),
				"count": FormUrlencodedParse(input).Count(),
			}
		case "byte_serialize":
			input, _ := hex.DecodeString(request.Hex)
			pieces := []string{}
			iterator := FormUrlencodedByteSerialize(input)
			for {
				piece, ok := iterator.Next().Get()
				if !ok {
					break
				}
				pieces = append(pieces, piece)
			}
			return map[string]any{"pieces": pieces, "text": FormUrlencodedByteSerialize(input).Collect()}
		case "form_serialize":
			serializer := FormUrlencodedSerializerForSuffix(FormUrlencodedTargetString(request.Target), int(request.Start))
			for _, call := range request.Calls {
				switch call.Call {
				case "clear":
					serializer.Clear()
				case "append_pair":
					serializer.AppendPair(call.Name, call.Value)
				case "append_key_only":
					serializer.AppendKeyOnly(call.Name)
				case "finish":
					serializer.Finish()
				default:
					panic("unknown form_serialize call " + call.Call)
				}
			}
			return map[string]any{"text": serializer.Finish()}
		case "from_file_path":
			path, _ := hex.DecodeString(request.Hex)
			// The corpus was recorded on Unix.
			out := map[string]any{"file": nil, "directory": nil}
			if url, err := urlFromFilePath(string(path), false); err == nil {
				out["file"] = url.AsStr()
			}
			if url, err := urlFromDirectoryPath(string(path), false); err == nil {
				out["directory"] = url.AsStr()
			}
			return out
		case "to_file_path":
			url, err := UrlParse(request.Input)
			if err != nil {
				return errorValue(err)
			}
			path, err := url.toFilePath(false)
			if err != nil {
				return map[string]any{"path": nil}
			}
			return map[string]any{"path": hex.EncodeToString([]byte(path))}
		case "serde":
			var serialized any
			if url, err := UrlParse(request.Input); err == nil {
				serialized = url.SerializeStr()
			}
			var deserialized any
			if url, err := UrlDeserializeStr(request.Input); err == nil {
				deserialized = map[string]any{"ok": url.AsStr()}
			} else {
				deserialized = map[string]any{"err": err.Error()}
			}
			return map[string]any{
				"serialized":   serialized,
				"deserialized": deserialized,
				// serde_json's words around the visitor's `expecting` text,
				// which is the part this package supplies.
				"wrong_type": "invalid type: integer `17`, expected " + UrlDeserializeExpecting + " at line 1 column 2",
			}
		case "debug":
			url, err := UrlParse(request.Input)
			if err != nil {
				return map[string]any{
					"err":    rs.Format("{error:?}", rs.Arg("error", err)),
					"padded": rs.Format("{error:>50}|{error:<12.6}", rs.Arg("error", err)),
				}
			}
			var host any
			if h, ok := url.Host().Get(); ok {
				host = rs.Format("{host:?}|{host:>30}|{host:<8.3}", rs.Arg("host", h))
			}
			return map[string]any{
				"debug":  rs.Format("{url:?}", rs.Arg("url", &url)),
				"pretty": rs.Format("{url:#?}", rs.Arg("url", &url)),
				"padded": rs.Format("{url:>60}|{url:<60}|{url:^7.5}", rs.Arg("url", &url)),
				"host":   host,
				"origin": strings.HasPrefix(rs.DebugString(url.Origin()), "Tuple"),
			}
		case "joining_probe":
			var classes strings.Builder
			accepts := func(text string) bool {
				_, err := HostParse(text)
				return err == nil
			}
			for code := request.Start; code < request.End; code++ {
				if !isScalarValue(code) {
					classes.WriteByte('-')
					continue
				}
				c := string(rune(code))
				bits := byte(0)
				for _, base := range joiningProbeBases {
					b := string(base)
					if accepts(c + "\u200C" + b) {
						bits |= 1
					}
					if accepts(b + "\u200C" + c) {
						bits |= 2
					}
					if accepts(b + c + "\u0301\u200C" + b) {
						bits |= 4
					}
				}
				classes.WriteByte('0' + bits)
			}
			return map[string]any{"classes": runLengths(classes.String())}
		default:
			panic("unknown op " + request.Op)
		}
	})
}

func asciiUpper(s string) string {
	b := []byte(s)
	for i, c := range b {
		if 'a' <= c && c <= 'z' {
			b[i] = c - ('a' - 'A')
		}
	}
	return string(b)
}

// ---------------------------------------------------------------------------
// Generators. All of them are deterministic: fixed lists, then choices made
// by a seeded generator.

// liveOracle reports whether the comparison runs against Rust now
// (MJ_ORACLE=live). The generators add their largest cases only then, which
// keeps the recorded corpora small; the cases recorded are a prefix of the
// cases generated live, so a recording is reproducible.
func liveOracle() bool {
	return os.Getenv("MJ_ORACLE") == "live"
}

type generator struct {
	random *rand.Rand
}

func newGenerator(seed uint64) *generator {
	return &generator{random: rand.New(rand.NewPCG(seed, 0x6d6a6f6c6e6972))}
}

func (g *generator) pick(options []string) string {
	return options[g.random.IntN(len(options))]
}

func (g *generator) chance(percent int) bool {
	return g.random.IntN(100) < percent
}

var genSchemes = []string{
	"http", "https", "ws", "wss", "ftp", "file", "sc", "data", "mailto", "blob", "javascript",
	"about", "git", "ssh", "a", "web+demo", "x.y-z+1", "non-spec", "HTTP", "hTtPs", "FILE", "Sc",
	"h\tttp", "1http", "ht tp", "http\u00e9", "", "", "",
}

var genAfterScheme = []string{
	"://", "://", "://", "://", ":", ":/", ":///", ":////", ":\\\\", ":\\", ":/\\", ":\\/", " :", "",
	":// ", ":/\t/", ":\n//",
}

var genUserinfo = []string{
	"", "", "", "", "", "user@", "user:pass@", ":pass@", "user:@", ":@", "@", "us%65r:p%40ss@",
	"u s:p\"w@", "a@b@", "a:b:c@", "\u00fc:\u00f1@", "user;x=y:pa=ss@", "u%zz:p%@", "u\ts:p\nw@",
	"[u]:{p}@", "u^|`:<p>@", "a\\b@", "USER:PASS@", "\u65e5\u672c@", "%@", "u:p:@",
}

var genHosts = []string{
	"example.com", "example.com", "example.com", "EXAMPLE.COM", "ExAmPlE.CoM.", "localhost", "LocalHost",
	"a.b.c", "h", "xn--bcher-kva.example", "b\u00fccher.example", "B\u00dcCHER.example", "\u2603.net",
	"\u65e5\u672c\u8a9e.jp", "fa\u00df.de", "\ufb00.com", "\uff11\uff12\uff17.\uff10.\uff10.\uff11",
	"127.0.0.1", "127.1", "0x7f.1", "0177.0.0.1", "2130706433", "0x7f000001", "1.2.3.4.5", "256.1.1.1",
	"1.2.3.", "1.2.3.4.", "1..2", "0xffffffff", "0x100000000", "4294967296", "999999999999", "08",
	"0x", "09.1.1.1", "0.0.0.0", "255.255.255.255", "255.255.255.256", "1.2.65536", "1.16777216",
	"0300.0250.0.1", "0xC0.0xA8.0x0.0x1", "192.168.0.1.", "192.168.0.1..", "1e3", "0x1.com", "com.0x1",
	"foo.09", "foo.0x", "foo.1e", "1.2.3.4.com", "a.1", "a.b.1.2", "1.2.3.0x", "0X7F.1", "00000000177.1",
	"[::1]", "[::]", "[2001:db8::1]", "[2001:DB8:0:0:8:800:200C:417A]", "[::ffff:1.2.3.4]", "[::1.2.3.4]",
	"[1:2:3:4:5:6:7:8]", "[1:2:3:4:5:6:7:8:9]", "[::1", "::1]", "[:::1]", "[1::2::3]", "[fe80::1%25eth0]",
	"[v1.x]", "[1.2.3.4]", "[0:0:0:0:0:0:0:0]", "[1:0:0:2:0:0:0:3]", "[::ffff:256.1.1.1]", "[::01.2.3.4]",
	"[1:2:3:4:5:6:1.2.3.4]", "[1:2:3:4:5:6:7:1.2.3.4]", "[::1.2.3]", "[::1.2.3.4.5]", "[12345::]",
	"[::g]", "[1:2:3:4:5:6:7::]", "[::2:3:4:5:6:7:8]", "[0:0:1:0:0:0:1:0]", "[ffff::]", "[:1]", "[1:]",
	"[]", "[::1]x", "[::ffff:0102:0304]", "[0::0:0:0]", "[a:b::c:1.2.3.4]",
	"", ".", "..", "a..b", "-a-.com", "a_b.com", "a b.com", "a%20b.com", "%41.com", "a%2Eb",
	"ex%61mple.com", "a%00b", "a<b", "a>b", "a^b", "a|b", "a\\b", "a[b", "a]b", "a{b}", "a`b", "a\"b",
	"a'b", "a!b$c&d", "a*b+c,d;e=f", "a~b", "xn--", "xn--a", "xn--a-", "xn---", "XN--BCHER-KVA",
	"xn--bcher-kva.XN--BCHER-KVA", "xn--e1afmkfd.xn--p1ai", "\u043f\u0440\u0438\u043c\u0435\u0440.\u0440\u0444",
	"\u0645\u062b\u0627\u0644.\u0645\u0635\u0631", "a.\u0645\u062b\u0627\u0644", "1\u0645\u062b\u0627\u0644",
	"a\u200cb", "\u0628\u200c\u0628", "a\u00adb", "a\u3002b", "a\uff0eb", "\uff41.com", "\u0301a", "a\u0301",
	"\u00df", "\u03c2", "\u0130", "\u01c5", "\u337f", "\u2488", "\u2460", "\u2122", "a\u2028b", "\ufffd",
	"\U0001F600", "r3---sn-apo3qvuoxuxbt-j5pe.googlevideo.com", "ab--c", "a%C3%BC", "%C3", "%FF", "a%2Fb",
	"a%3Ab", "a%40b", "a%5Bb", "x%2e", "%2e%2e", "%31%32%37.1", "%30x7f.1", "a.b.c.d.e.f.g.h.i.j",
	"xn--bcher-kva", "xn--b\u00fccher", "xn--nxasmq6b", "xn--zca", "xn--80ak6aa92e.com", "\u0430\u0440\u0440\u04cf\u0435.com",
	"a\u0308.com", "\u1e9e.de", "\u0646\u0627\u0645\u0647\u200c\u0627\u06cc", "\u0915\u094d\u200d\u0937", "\u05d0.\u05d1", "\u05d01", "1\u05d0",
	"\u0660\u0661", "a\u06601", "www.example.com.", "example.com..", ".example.com", "*.example.com",
	"_dmarc.example.com", "sub_domain.example", "EXAMPLE", "Ex", "x", "0", "1", "-", "a-", "-a",
}

var genPorts = []string{
	"", "", "", "", "", ":", ":80", ":443", ":21", ":0", ":8080", ":65535", ":65536", ":99999", ":080",
	":00080", ":8a", ":-1", ": 80", ":80 ", ":\t80", ":8\n0", ":\u0663", ":+80", ":000000000000000000443",
	":4 4 3", ":1:2", ":0x50",
}

var genPaths = []string{
	"", "", "/", "/", "/a", "/a/b/c", "/a/./b", "/a/../b", "/a/../../b", "/..", "/../", "/./", "/.",
	"/a/%2e/b", "/a/%2E%2e/b", "/a/.%2e/b", "/a/%2e./b", "/a/%2e%2E", "/a/..%2f", "//", "///a", "/a//b",
	"\\", "\\a\\b", "/a\\b", "/a\\..\\b", "/C:/", "/C|/", "/c:", "/C:/../x", "/a b", "/a\tb", "/a\nb",
	"/\u00fc", "/%C3%BC", "/%zz", "/%", "/%4", "/<>\"`{}", "/[]^|", "/~!$&'()*+,;=:@", "/a;b=c", "/\x7f",
	"/\x00", "/\U0001F600", "/a/", "/a/b/", "a", "a/b", "a b ", "a  ", "/.//x", "/..//x", "/a/..//x",
	"/C:", "/C:\\x", "/c|/x/../..", "//C:/x", "/a/b/../../../c", "/%2e%2e/%2E%2E/x", "/a/b/..", "/a/b/.",
	"/a/b/%2e", "/a/b/%2e%2e/", "/\u65e5\u672c/\u8a9e", "/a%2Fb", "/a%5Cb", "/%2E", "/.../", "/..a",
	"/a../b", "/.a", "/a.", "/x/../y/./z/..", "/ ", "/%20", "/+", "/a:b", "/a@b", "x:y", ".", "..", "./a",
	"../a", "\\..\\a", "/\\../a", "/a/\\../b",
}

var genQueries = []string{
	"", "", "", "?", "?a=b", "?a=b&c=d", "?a b", "?a'b", "?a\"b<c>d", "?a#b", "?\u00fc", "?%C3%BC",
	"?%zz", "?a\tb", "?a\nb", "??", "?a?b", "?&&=&", "?a=b;c=d", "?+ +", "?\x7f", "?[]{}|\\^`", "?a=%26&b",
	"?\u65e5=\u672c", "?a=b=c", "?=", "?&", "?a&&b", "?%", "?a%2", "?\x00", "? ", "?/..", "?\\",
}

var genFragments = []string{
	"", "", "", "#", "#a", "#a b", "#a\"b<c>d`e", "#\u00fc", "#%zz", "#a#b", "#\x00", "#a\tb", "#{}|\\^",
	"#?", "#/..", "#%", "# ", "#\u65e5", "#'", "#a%20b",
}

var genWrappers = [][2]string{
	{" ", " "}, {"\t\n", "\r"}, {"\x00\x1f", "\x1f"}, {"  \t ", ""}, {"", "  "}, {"\x01", ""}, {"\u00a0", "\u00a0"},
	{"\n", "\n"}, {"", "\x00"},
}

var genCommonSchemes = []string{"http", "https", "ws", "wss", "ftp", "file", "sc", "git", "ssh", "web+demo"}

var genCommonHosts = []string{
	"example.com", "EXAMPLE.COM", "localhost", "a.b.c", "h", "127.0.0.1", "[::1]", "b\u00fccher.example",
	"xn--bcher-kva.example", "\u2603.net", "0x7f.1", "[2001:db8::1]", "a_b.com", "www.example.com.",
}

var genCommonPorts = []string{"", "", "", ":80", ":443", ":8080", ":65535", ":0", ":21"}

// pickMostly takes from common most of the time and from all otherwise.
func (g *generator) pickMostly(percent int, common []string, all []string) string {
	if g.chance(percent) {
		return g.pick(common)
	}
	return g.pick(all)
}

// url builds one URL from the lists. Most are well-formed; each part is one
// of the odd spellings often enough that every pair of them meets.
func (g *generator) url() string {
	scheme := g.pickMostly(75, genCommonSchemes, genSchemes)
	if scheme != "" && g.chance(15) {
		scheme = flipCase(g, scheme)
	}
	var out strings.Builder
	wrapper := [2]string{"", ""}
	if g.chance(12) {
		wrapper = genWrappers[g.random.IntN(len(genWrappers))]
	}
	out.WriteString(wrapper[0])
	out.WriteString(scheme)
	after := g.pickMostly(70, []string{"://"}, genAfterScheme)
	if scheme == "" {
		after = g.pick([]string{"", "", "//", "/", "\\\\", "///"})
	}
	out.WriteString(after)
	if strings.Contains(after, "/") || strings.Contains(after, "\\") || g.chance(20) {
		out.WriteString(g.pickMostly(70, []string{""}, genUserinfo))
		out.WriteString(g.pickMostly(60, genCommonHosts, genHosts))
		out.WriteString(g.pickMostly(75, genCommonPorts, genPorts))
	}
	out.WriteString(g.pick(genPaths))
	out.WriteString(g.pick(genQueries))
	out.WriteString(g.pick(genFragments))
	out.WriteString(wrapper[1])
	text := out.String()
	if g.chance(6) {
		text = g.sprinkle(text, "\t\n\r")
	}
	return text
}

func flipCase(g *generator, s string) string {
	b := []byte(s)
	for i, c := range b {
		if g.chance(50) {
			switch {
			case 'a' <= c && c <= 'z':
				b[i] = c - 32
			case 'A' <= c && c <= 'Z':
				b[i] = c + 32
			}
		}
	}
	return string(b)
}

// sprinkle inserts a few of the characters at character boundaries.
func (g *generator) sprinkle(text string, characters string) string {
	runes := []rune(text)
	for range 1 + g.random.IntN(3) {
		position := g.random.IntN(len(runes) + 1)
		c := rune(characters[g.random.IntN(len(characters))])
		runes = append(runes[:position], append([]rune{c}, runes[position:]...)...)
	}
	return string(runes)
}

var genAlphabet = []string{
	"a", "b", "c", "x", "Z", "0", "1", "9", ".", ".", "/", "/", "\\", ":", ":", "@", "?", "#", "%", "%2", "%2e",
	"%2E", "%41", "%zz", "[", "]", "-", "_", "~", "+", " ", "\t", "\n", "'", "\"", "<", ">", "^", "`", "{", "}", "|",
	"=", "&", ";", ",", "!", "$", "*", "(", ")", "\u00fc", "\u65e5", "\U0001F600", "\x00", "\x7f", "\u0080",
	"xn--", "http", "file", "C|", "c:", "..", "//", "::", "0x", "\u00df", "\u200c", "\u0628", "\u3002",
}

// noise builds a string from characters and fragments that mean something
// to the parser, in no order.
func (g *generator) noise(maxParts int) string {
	var out strings.Builder
	for range g.random.IntN(maxParts + 1) {
		out.WriteString(g.pick(genAlphabet))
	}
	return out.String()
}

var genBases = []string{
	"http://example.com/a/b/c?q#f", "http://example.com", "http://user:pw@example.com:8080/a/b/",
	"https://example.com/a/b/../c", "file:///a/b/c", "file:///C:/a/b", "file://host/share/file", "file:///",
	"file://localhost/x", "sc://host/a/b", "sc://host", "sc:/a/b", "sc:a/b", "data:text/plain,hi", "mailto:x@y",
	"sc://", "sc:///a", "web+demo:/.//not-a-host/", "sc:/.//x", "ws://h/a?b", "ftp://h/%2e/",
	"blob:http://example.com/uuid", "about:blank", "http://[::1]:8080/p", "http://127.0.0.1/", "file:///C|/x",
	"file:c:/x", "sc://%C3%B1/x", "http://example.com/a/b/c/", "file://host/C:/dir/file", "file:///C:",
	"sc://user:pw@host:99/p?q#f", "http://example.com/a/b?c/d#e/f", "sc:/a", "sc:/", "sc://h/", "sc://h?q",
	"sc://h#f", "wss://example.com:443/chat", "https://xn--bcher-kva.example/x/", "file:///a/b/",
	"http://example.com/%2e%2e/x", "non-spec:/.//p", "sc:///", "file://h/", "x:/a/b/c/..", "http://h/a/b/c/d/e/f",
	"sc://host:99/p", "sc://h:1", "sc://[::1]/x", "sc://1.2.3.4:5/x?q", "http://user@example.com/", "ssh://git@github.com:22/o/r.git",
	"https://api.github.com/", "http://localhost:6767/foo/bar?a=b", "postgres://postgres@localhost/",
}

var genReferences = []string{
	"", ".", "./", "..", "../", "../..", "../../..", "../x", "./x", "x", "x/", "x/y", "x/../y", "/", "/x",
	"/x/../y", "//", "//h", "//h/p", "//h:99/p", "//user@h/p", "///x", "////x", "\\\\h\\p", "\\x", "/\\x",
	"\\/x", "?", "?q", "?q#f", "#", "#f", "##", "x?q", "x#f", "http:", "http:x", "http:/x", "http://x",
	"http:///x", "https:x", "file:", "file:x", "file:/x", "file://x", "file:///x", "file:..", "sc:x", "sc:/x",
	"sc://x", "C:", "C:/", "C|", "C|/x", "c:\\x", "/C:/x", "/c|/x", "//C:/x", "%2e", "%2e%2e", "%2E/x",
	".%2e/x", "..%2f", ".../x", "x/./y", "x//y", " x ", "\tx\n", "x y", "\u00fc", "%zz", ";x", ":x", "1:x",
	"a:b", "a+b:c", "x:", "/x:y", "x/y:z", "data:,x", "mailto:a@b", "javascript:alert(1)", "#\x00", "?\u00a0",
	"x\x00y", "//", "//:99", "//@", "//u@", "//[::1]", "//[::1", "//1.2.3.4.5/", "//x:y@z", "/.//p", "/..//p",
	"..//p", ".//p", "//p", "x/..//p", "?#", "#?", "\\", "\\\\", "\\\\\\x", "/a/b/c/../../../../d", "..\\..\\x",
	"C|#", "C|?", "C|\\", "C|\n/", "/c:/foo/bar", "file:C:/", "file:/C:/", "file:.//p", "file:/.//p", "////one/two",
	"FILE:x", "HtTp:x", "ws:x", "sc:", "sc:#f", "sc:?q", "x\ty", "\x1fx", "x\u3000",
}

// ---------------------------------------------------------------------------
// The comparisons.

// TestDiffAgainstRust_parse compares everything a parsed URL says about
// itself.
func TestDiffAgainstRust_parse(t *testing.T) {
	withoutDebugAssertions(t)
	oracle.Check(t, "url", "url_parse", func(yield func(any)) {
		fixed := []string{
			"", " ", "http://example.com", "HTTP://USER:PW@EXAMPLE.COM:80/a/../b?x=1#f", "https://example.com:443/",
			"http://example.com:8080", "sc://", "sc://a", "sc:", "sc:a", "sc:/", "sc:///", "data:text/plain,Hello?World#",
			"mailto:test@example.net", "file:///tmp/foo", "file://localhost/tmp", "file://host/C:/x", "file:///C|/x",
			"file:c:\\x", "blob:http://example.net/", "blob:malformed//", "http://[::1]/", "http://[:::1]",
			"http://1.35.33.49", "http://0x1232131", "http://\u2603.net/", "http://\U0001F605.com", "http://user@/",
			"http://:@/", "http://@:80", "sc://@/", "sc://:80/", "sc://a:8080/p", "m:/.//\\", "web+demo:/.//not-a-host/",
			"web+demo:/path/..//not-a-host/", "arhttpsps:/.//eom/dae.com/\\\\t\\:", "l://\x01:", "fIlE:p:?../",
			"http:////mozilla.org:42", "http://mozilla.org\\path", "http://example.com/\ud7ff\ue000?\ufdd0#\ufffe",
			"http://example.com/%00%7f%80%ff", "http://h/a?b#c d", "x://h/?'", "http://h/?'", "ws://h/?'", "non-special://h/p a t h",
			"../main.css", "//example.com", "/path", "?query", "#fragment",
		}
		for _, input := range fixed {
			yield(map[string]any{"op": "parse", "input": input})
		}
		g := newGenerator(1)
		for range 700 {
			yield(map[string]any{"op": "parse", "input": g.url()})
		}
	}, answer)
}

// TestDiffAgainstRust_parse_components compares the components of many more
// inputs, most of them not URLs at all.
func TestDiffAgainstRust_parse_components(t *testing.T) {
	withoutDebugAssertions(t)
	oracle.Check(t, "url", "url_parse_components", func(yield func(any)) {
		g := newGenerator(2)
		for range 4500 {
			yield(map[string]any{"op": "parse", "input": g.url(), "brief": true})
		}
		for range 1000 {
			yield(map[string]any{"op": "parse", "input": g.sprinkle(g.url(), "\t\n\r \x00\\/:@?#%."), "brief": true})
		}
		for range 1000 {
			yield(map[string]any{"op": "parse", "input": g.noise(12), "brief": true})
		}
		for range 2500 {
			yield(map[string]any{"op": "parse", "input": g.pickMostly(80, genCommonSchemes, genSchemes) + g.pickMostly(60, []string{"://", ":"}, genAfterScheme) + g.noise(10), "brief": true})
		}
	}, answer)
}

// TestDiffAgainstRust_join compares relative references resolved against
// bases of every kind.
func TestDiffAgainstRust_join(t *testing.T) {
	withoutDebugAssertions(t)
	oracle.Check(t, "url", "url_join", func(yield func(any)) {
		for _, base := range genBases {
			for _, reference := range genReferences {
				yield(map[string]any{"op": "parse", "input": reference, "base": base, "brief": true})
			}
		}
		g := newGenerator(3)
		for range 1500 {
			base := g.pick(genBases)
			if g.chance(40) {
				base = g.url()
			}
			var reference string
			switch g.random.IntN(4) {
			case 0:
				reference = g.url()
			case 1:
				reference = g.noise(8)
			case 2:
				reference = g.pick(genPaths) + g.pick(genQueries) + g.pick(genFragments)
			default:
				reference = g.pick(genReferences) + g.noise(3)
			}
			yield(map[string]any{"op": "parse", "input": reference, "base": base, "brief": true})
		}
		for range 250 {
			base := g.pick(genBases)
			if g.chance(40) {
				base = g.url()
			}
			yield(map[string]any{"op": "parse", "input": g.pick(genReferences), "base": base})
		}
	}, answer)
}

var genSetterValues = map[string][]string{
	"fragment":        {"", "f", "a b", "a#b", "\u00fc", "\x00", "a\tb", "%zz", "<>\"`", "#f", " x "},
	"query":           {"", "q", "a=b&c", "a b", "a'b", "a#b", "\u00fc", "\ta\n", "?x", "a\tb", "<>\"", " x "},
	"path":            {"", "/", "x", "/x/y", "x/../y", "/..", "a b", "a?b", "a#b", "\\x", "//x", "/C:/x", "%2e", "\u00fc", "/x/", "..", ".", "/.//x", "x\ty", "%2F", "/a/%2e%2e/b", " "},
	"host":            {"", "example.org", "EXAMPLE.org", "b\u00fccher.example", "127.1", "[::1]", "[::1", "x:99", ":99", "a b", "a/b", "a?b", "a#b", "a@b", "xn--", "%41", "\u00e4%41", "0x7f.1", "1.2.3.4.5", "*", "x.", "[::1]:80", "a\tb", "localhost", "a:b:c", "[1::2]x", "\u2603"},
	"ip_host":         {"127.0.0.1", "::1", "::", "0.0.0.0", "2001:db8::8:800:200c:417a", "::ffff:1.2.3.4", "255.255.255.255", "1:2:3:4:5:6:7:8"},
	"password":        {"", "pw", "p@ss", "p:w", "p w", "\u00fc", "%41", "p/w", "p?w#", "[p]", "\tp"},
	"username":        {"", "user", "us er", "u:s", "u@s", "\u00fc", "%41", "u/s", "u?s#", "[u]", "\tu"},
	"scheme":          {"http", "https", "ws", "wss", "ftp", "file", "sc", "a", "A+b-c.d", "1a", "", "a:", "a b", "http:", "\u00fc", "x:y", "HTTPS", "a\tb", "git", "blob"},
	"quirks_href":     {"http://x/", "bad", "sc:x", "", "file:///y", "//z"},
	"quirks_protocol": {"http", "https:", "file:", "sc:", "a:b:c", "1a:", "", ":", "WSS", "ftp://x", "a b:"},
	"quirks_username": {"", "user", "us er", "u:s", "u@s", "\u00fc", "%41"},
	"quirks_password": {"", "pw", "p@ss", "p:w", "p w", "\u00fc", "%41"},
	"quirks_host":     {"", "h", "h:99", "h:", "h:99x", "h:x", ":99", "[::1]:99", "h/p", "h?q", "h#f", "h\\p", "a b", "x:65536", "x:0", "\u00e4", "H.EXAMPLE", "127.0.0.1:80", "h:80", "h:443", "[::1", "x:\t9", "a@b", "x: 9", "h:99/p"},
	"quirks_hostname": {"", "h", "h:99", "[::1]", "h/p", "a b", "\u00e4", "H.EXAMPLE", "0x7f.1", "h?q", "h#f", "h\\p", "a@b", "x\ty"},
	"quirks_port":     {"", "80", "8080", "0", "65535", "65536", "8a", "a8", " 80", "+80", "\x009000", "443", "21", "8\t0", "99999999999", "080", "8/0", "8?0", "8#0", "8\\0"},
	"quirks_pathname": {"", "/", "x", "/x", "\\x", "//x", "/.//x", "/..//x", "p", "?", "#", "a b", "/a/../b", "\\\\", "//\\/", "//monkey/..//", "%2e", "/C:/x"},
	"quirks_search":   {"", "?", "q", "?q", "??q", "a b", "#x", "a'b", "\tq\n", "\u00fc"},
	"quirks_hash":     {"", "#", "f", "#f", "##f", "a b", "\x00", "\u00fc", "a\tb"},
}

var genSetterNames = []string{
	"fragment", "query", "path", "port", "host", "ip_host", "password", "username", "scheme", "quirks_href",
	"quirks_protocol", "quirks_username", "quirks_password", "quirks_host", "quirks_hostname", "quirks_port",
	"quirks_pathname", "quirks_search", "quirks_hash", "path_segments", "query_pairs",
	// The setters Mjolnir calls come up more often.
	"path", "query", "port", "username", "password", "path_segments", "query_pairs", "ip_host",
}

var genSegments = []string{
	"", "a", "b c", "..", ".", "a/b", "a\\b", "%2e", "%", "?", "#", "\u00fc", "C:", "c|", "x\ty", "a b", "%2F", " ", "..%2e", "~", "a:b",
}

var genPairParts = []string{
	"", "a", "b c", "a&b", "a=b", "a+b", "\u00fc", "%41", "#", "?", "*-._", "~", "\x00", "a\nb", "\U0001F600", "'\"", "/:@", "caf\u00e9",
}

func (g *generator) setterStep() map[string]any {
	name := g.pick(genSetterNames)
	step := map[string]any{"set": name}
	switch name {
	case "port":
		if g.chance(75) {
			ports := []int{80, 443, 21, 8080, 0, 65535, 22, 8443}
			step["port"] = ports[g.random.IntN(len(ports))]
		}
	case "fragment", "query", "host", "password":
		if g.chance(80) {
			step["value"] = g.pick(genSetterValues[name])
		}
	case "path_segments":
		calls := []any{}
		for range 1 + g.random.IntN(4) {
			switch g.random.IntN(6) {
			case 0:
				calls = append(calls, map[string]any{"call": "clear"})
			case 1:
				calls = append(calls, map[string]any{"call": "pop"})
			case 2:
				calls = append(calls, map[string]any{"call": "pop_if_empty"})
			case 3, 4:
				calls = append(calls, map[string]any{"call": "push", "segment": g.pick(genSegments)})
			default:
				segments := []string{}
				for range g.random.IntN(4) {
					segments = append(segments, g.pick(genSegments))
				}
				calls = append(calls, map[string]any{"call": "extend", "segments": segments})
			}
		}
		step["calls"] = calls
	case "query_pairs":
		calls := []any{}
		for range 1 + g.random.IntN(4) {
			switch g.random.IntN(8) {
			case 0:
				calls = append(calls, map[string]any{"call": "clear"})
			case 1, 2, 3:
				calls = append(calls, map[string]any{"call": "append_pair", "name": g.pick(genPairParts), "value": g.pick(genPairParts)})
			case 4:
				calls = append(calls, map[string]any{"call": "append_key_only", "name": g.pick(genPairParts)})
			case 5:
				pairs := [][]string{}
				for range g.random.IntN(3) {
					pairs = append(pairs, []string{g.pick(genPairParts), g.pick(genPairParts)})
				}
				calls = append(calls, map[string]any{"call": "extend_pairs", "pairs": pairs})
			case 6:
				keys := []string{}
				for range g.random.IntN(3) {
					keys = append(keys, g.pick(genPairParts))
				}
				calls = append(calls, map[string]any{"call": "extend_keys_only", "keys": keys})
			default:
				// A serializer that is finished refuses every later call.
				if g.chance(30) {
					calls = append(calls, map[string]any{"call": "finish"})
				}
			}
		}
		step["calls"] = calls
	default:
		step["value"] = g.pick(genSetterValues[name])
	}
	return step
}

// TestDiffAgainstRust_setters compares the URL after every step of sequences
// of setter calls.
func TestDiffAgainstRust_setters(t *testing.T) {
	withoutDebugAssertions(t)
	oracle.Check(t, "url", "url_setters", func(yield func(any)) {
		g := newGenerator(4)
		// Every setter with every value, on a few URLs of each kind.
		starts := []string{
			"http://user:pw@example.com:8080/a/b?q=1#f", "https://example.com/", "file:///a/b", "file://host/a",
			"sc://user:pw@host:99/a/b?q#f", "sc:/a/b", "sc:opaque?q#f", "sc://", "ws://h",
		}
		for _, name := range []string{
			"fragment", "query", "path", "host", "ip_host", "password", "username", "scheme", "quirks_href",
			"quirks_protocol", "quirks_username", "quirks_password", "quirks_host", "quirks_hostname", "quirks_port",
			"quirks_pathname", "quirks_search", "quirks_hash",
		} {
			for _, value := range genSetterValues[name] {
				for _, href := range starts {
					yield(map[string]any{"op": "set", "href": href, "brief": true, "steps": []any{
						map[string]any{"set": name, "value": value},
					}})
				}
			}
		}
		for _, name := range []string{"fragment", "query", "host", "password", "port"} {
			for _, href := range append(starts, genBases...) {
				yield(map[string]any{"op": "set", "href": href, "brief": true, "steps": []any{map[string]any{"set": name}}})
			}
		}
		for _, port := range []int{80, 443, 21, 8080, 0, 65535} {
			for _, href := range append(starts, genBases...) {
				yield(map[string]any{"op": "set", "href": href, "brief": true, "steps": []any{map[string]any{"set": "port", "port": port}}})
			}
		}
		for range 2200 {
			href := g.pick(genBases)
			if g.chance(35) {
				href = g.url()
			}
			steps := []any{}
			for range 1 + g.random.IntN(4) {
				steps = append(steps, g.setterStep())
			}
			yield(map[string]any{"op": "set", "href": href, "brief": true, "steps": steps})
		}
		for range 250 {
			href := g.pick(genBases)
			steps := []any{}
			for range 1 + g.random.IntN(3) {
				steps = append(steps, g.setterStep())
			}
			yield(map[string]any{"op": "set", "href": href, "steps": steps})
		}
	}, answer)
}

// TestDiffAgainstRust_host compares the host parsers and the two domain
// conversions.
func TestDiffAgainstRust_host(t *testing.T) {
	withoutDebugAssertions(t)
	oracle.Check(t, "url", "url_host", func(yield func(any)) {
		for _, host := range genHosts {
			yield(map[string]any{"op": "host_parse", "input": host})
			yield(map[string]any{"op": "host_parse", "input": host, "opaque": true})
			yield(map[string]any{"op": "domain", "input": host})
		}
		g := newGenerator(5)
		for range 1500 {
			var host string
			switch g.random.IntN(4) {
			case 0:
				host = g.pick(genHosts) + "." + g.pick(genHosts)
			case 1:
				host = g.noise(6)
			case 2:
				host = g.sprinkle(g.pick(genHosts), "\u200c\u200d\u0301\u0628\u05d0.-\u00adA1\u3002\u094d")
			default:
				host = flipCase(g, g.pick(genHosts))
			}
			yield(map[string]any{"op": "host_parse", "input": host})
			if g.chance(30) {
				yield(map[string]any{"op": "host_parse", "input": host, "opaque": true})
			}
			if g.chance(50) {
				yield(map[string]any{"op": "domain", "input": host})
			}
		}
		// IPv4 numbers and IPv6 addresses in bulk.
		digits := []string{"0", "1", "7", "8", "9", "a", "f", "F", "x", "X", ".", ".", "0x", "255", "256", "65535", "08", "00", "g"}
		for range 800 {
			var host strings.Builder
			for range 1 + g.random.IntN(9) {
				host.WriteString(g.pick(digits))
			}
			yield(map[string]any{"op": "host_parse", "input": host.String()})
		}
		pieces := []string{"0", "1", "ffff", "FFFF", "abcd", "12345", ":", ":", "::", "1.2.3.4", "255.255.255.255", "256.1.1.1", "01.2.3.4", ".", "g", "0000", "8"}
		for range 1200 {
			var host strings.Builder
			host.WriteString("[")
			for range 1 + g.random.IntN(10) {
				host.WriteString(g.pick(pieces))
				if g.chance(60) {
					host.WriteString(":")
				}
			}
			host.WriteString("]")
			yield(map[string]any{"op": "host_parse", "input": host.String()})
		}
	}, answer)
}

// idnaTestInputs reads the source column of the conformance test file the
// idna crate ships, IdnaTestV2.txt. It is needed only when the corpus is
// recorded or compared live.
func idnaTestInputs(t *testing.T) []string {
	matches, err := filepath.Glob(filepath.Join(os.Getenv("HOME"), ".cargo", "registry", "src", "*", "idna-1.1.0", "tests", "IdnaTestV2.txt"))
	if err != nil || len(matches) == 0 {
		t.Fatalf("idna-1.1.0/tests/IdnaTestV2.txt is not in the Cargo registry; run go/port/fetch-reference.sh")
	}
	text, err := os.ReadFile(matches[0])
	if err != nil {
		t.Fatal(err)
	}
	var inputs []string
	seen := map[string]bool{}
	for _, line := range strings.Split(string(text), "\n") {
		if comment := strings.IndexByte(line, '#'); comment >= 0 {
			line = line[:comment]
		}
		if strings.TrimSpace(line) == "" {
			continue
		}
		source := strings.TrimSpace(strings.Split(line, ";")[0])
		source = unescapeIdnaTest(source)
		if !seen[source] {
			seen[source] = true
			inputs = append(inputs, source)
		}
	}
	return inputs
}

// unescapeIdnaTest decodes the \uXXXX and \x{XXXX} escapes of IdnaTestV2.txt,
// and its `""` for the empty string. An escaped surrogate cannot be in a
// string and becomes U+FFFD.
func unescapeIdnaTest(source string) string {
	if source == `""` {
		return ""
	}
	var out strings.Builder
	for i := 0; i < len(source); {
		if strings.HasPrefix(source[i:], `\u`) && i+6 <= len(source) {
			var code rune
			if _, err := fmt.Sscanf(source[i+2:i+6], "%04x", &code); err == nil {
				out.WriteRune(code)
				i += 6
				continue
			}
		}
		if strings.HasPrefix(source[i:], `\x{`) {
			if end := strings.IndexByte(source[i:], '}'); end > 0 {
				var code rune
				if _, err := fmt.Sscanf(source[i+3:i+end], "%x", &code); err == nil {
					out.WriteRune(code)
					i += end + 1
					continue
				}
			}
		}
		out.WriteByte(source[i])
		i++
	}
	return out.String()
}

// TestDiffAgainstRust_idna_conformance compares the two domain conversions on
// every source string of Unicode's IDNA conformance tests.
func TestDiffAgainstRust_idna_conformance(t *testing.T) {
	withoutDebugAssertions(t)
	oracle.Check(t, "url", "url_idna_conformance", func(yield func(any)) {
		for _, input := range idnaTestInputs(t) {
			yield(map[string]any{"op": "domain", "input": input})
		}
	}, answer)
}

// hostDigestContexts are the texts every scalar value is put between and
// parsed as a host.
var hostDigestContexts = [][2]string{
	{"", ""},
	{"a", "b"},
	{"\u0628", ""},
	{"xn--", ""},
}

// TestDiffAgainstRust_host_every_scalar_value parses a host holding each
// Unicode scalar value, alone and next to other characters, and compares a
// digest of the answers for every 4096 of them. It is what ties the Go
// Unicode tables (mapping, validity, normalization, marks, Bidi classes) to
// the ICU4X data the Rust crate is built with.
func TestDiffAgainstRust_host_every_scalar_value(t *testing.T) {
	withoutDebugAssertions(t)
	oracle.Check(t, "url", "url_host_digest", func(yield func(any)) {
		for _, context := range hostDigestContexts {
			for start := 0; start < 0x110000; start += 0x1000 {
				yield(map[string]any{"op": "host_digest", "prefix": context[0], "suffix": context[1], "start": start, "end": start + 0x1000})
			}
		}
	}, answer)
}

// TestDiffAgainstRust_joining_probe asks the ContextJ questions that
// idna_joining_table.go was generated from, for every scalar value.
func TestDiffAgainstRust_joining_probe(t *testing.T) {
	withoutDebugAssertions(t)
	oracle.Check(t, "url", "url_joining_probe", func(yield func(any)) {
		for start := 0; start < 0x110000; start += joiningProbeChunk {
			yield(map[string]any{"op": "joining_probe", "start": start, "end": start + joiningProbeChunk})
		}
	}, answer)
}

// TestDiffAgainstRust_form_urlencoded compares the form_urlencoded parser
// and serializers on arbitrary bytes, UTF-8 or not.
func TestDiffAgainstRust_form_urlencoded(t *testing.T) {
	withoutDebugAssertions(t)
	oracle.Check(t, "url", "url_form_urlencoded", func(yield func(any)) {
		fixed := []string{
			"", "a", "a=b", "a=b&c=d", "&&", "=", "a=", "=b", "a=b=c", "a+b=c+d", "%41=%42", "%4=%", "%zz=%%", "a&b&c",
			"caf\u00e9=\u65e5\u672c", "%C3%A9=%E6%97%A5", "%FF=%C3", "\xff\xfe=\xc3", "a=\xe2\x82", "\xf0\x9f=\xf0\x9f\x98", "a;b=c",
			"a=b#c", "+=+", "%2B=%26", "\x00=\x01", "a=b&", "&a=b", "a==b", "a=b&&c=d", "%E2%82%AC=\xed\xa0\x80", "\xc0\xaf=\xf4\x90\x80\x80",
		}
		for _, input := range fixed {
			yield(map[string]any{"op": "form_parse", "hex": hex.EncodeToString([]byte(input))})
			yield(map[string]any{"op": "byte_serialize", "hex": hex.EncodeToString([]byte(input))})
		}
		for b := range 256 {
			yield(map[string]any{"op": "byte_serialize", "hex": hex.EncodeToString([]byte{byte(b)})})
		}
		g := newGenerator(6)
		parts := []string{"a", "b", "=", "=", "&", "&", "+", "%", "%4", "%41", "%zz", "%C3", "%A9", "%FF", "\xc3", "\xa9", "\xe2\x82", "\xff", "\xf0\x9f\x98\x80", "\xed\xa0\x80", " ", ";", "#", "\x00", "\u00e9", "*", "-", ".", "_", "~", "/"}
		for range 1500 {
			var input strings.Builder
			for range g.random.IntN(14) {
				input.WriteString(g.pick(parts))
			}
			yield(map[string]any{"op": "form_parse", "hex": hex.EncodeToString([]byte(input.String()))})
			if g.chance(50) {
				yield(map[string]any{"op": "byte_serialize", "hex": hex.EncodeToString([]byte(input.String()))})
			}
		}
		// Long inputs, past any buffer a streaming implementation might use.
		// The recorded corpus keeps one; the live comparison runs all three.
		sizes := []int{70000}
		if liveOracle() {
			sizes = append(sizes, 200000, 1<<20)
		}
		for _, size := range sizes {
			var input strings.Builder
			for input.Len() < size {
				input.WriteString(g.pick(parts))
			}
			yield(map[string]any{"op": "byte_serialize", "hex": hex.EncodeToString([]byte(input.String()))})
		}
		for range 500 {
			calls := []any{}
			for range g.random.IntN(6) {
				switch g.random.IntN(8) {
				case 0:
					calls = append(calls, map[string]any{"call": "clear"})
				case 1:
					calls = append(calls, map[string]any{"call": "append_key_only", "name": g.pick(genPairParts)})
				case 2:
					if g.chance(20) {
						calls = append(calls, map[string]any{"call": "finish"})
					}
				default:
					calls = append(calls, map[string]any{"call": "append_pair", "name": g.pick(genPairParts), "value": g.pick(genPairParts)})
				}
			}
			target := g.pick([]string{"", "", "x=1", "http://h/?", "abc"})
			start := g.random.IntN(len(target) + 2)
			yield(map[string]any{"op": "form_serialize", "target": target, "start": start, "calls": calls})
		}
	}, answer)
}

// TestDiffAgainstRust_file_path compares the conversions between `file:` URLs
// and Unix paths. (The Windows forms are tested from the crate's own tests:
// the oracle runs where it is built.)
func TestDiffAgainstRust_file_path(t *testing.T) {
	withoutDebugAssertions(t)
	oracle.Check(t, "url", "url_file_path", func(yield func(any)) {
		paths := []string{
			"/", "/foo/bar", "/foo/ba\x00r", "/foo/ba%00r", "/foo/ba\\r", "/foo/ba\x80r", "relative", "../relative", "",
			"/a b/c?d#e", "/a/./b", "/a/../b", "/a//b/", "/./a", "/..", "/a/.", "/a/..", "//a", "///", "/c:/", "/C|/x",
			"/\u00fc/\u65e5\u672c", "/a/b/", "/%2e/%2E%2e", "/~user/[x]/{y}", "/a;b=c:d@e", "/<>\"`^|", ".", "./a", "a/b",
			"/\xff\xfe", "/a\tb\nc", "/ ", "/a/ /b",
		}
		for _, path := range paths {
			yield(map[string]any{"op": "from_file_path", "hex": hex.EncodeToString([]byte(path))})
		}
		urls := []string{
			"file:///", "file:///foo/bar", "file:///foo/ba%00r", "file:///foo/ba%80r", "file://localhost/x", "file://host/x",
			"file:///C:/x", "file:///c:", "file:///c|", "file:///a/c:", "file:///x/c|", "http://example.com/a/b", "http://localhost/a",
			"sc:/a/b", "sc:opaque", "sc://h/p", "sc:///p", "file:///a%2Fb", "file:///a%2fb/%2e%2e", "file:///%E6%97%A5", "file:///a?q#f",
			"file:///a/b/", "file:////a", "file:///a//b", "mailto:x", "data:,x", "file:///%", "file:///%zz", "file://127.0.0.1/x",
			"file://[::1]/x", "file:///ab", "file:///a:", "file:///1:", "ftp://h/c:",
		}
		for _, url := range urls {
			yield(map[string]any{"op": "to_file_path", "input": url})
		}
		g := newGenerator(7)
		parts := []string{"/", "/", "a", "b c", ".", "..", "%", "%2e", "\\", "?", "#", "\u00fc", "\xff", "c:", "C|", ":", "~", "\x00"}
		for range 600 {
			var path strings.Builder
			if g.chance(85) {
				path.WriteString("/")
			}
			for range g.random.IntN(8) {
				path.WriteString(g.pick(parts))
			}
			yield(map[string]any{"op": "from_file_path", "hex": hex.EncodeToString([]byte(path.String()))})
		}
		for range 600 {
			yield(map[string]any{"op": "to_file_path", "input": "file://" + g.pick([]string{"", "", "", "localhost", "host", "LOCALHOST"}) + g.pick(genPaths) + g.pick(genQueries)})
		}
	}, answer)
}

// TestDiffAgainstRust_violations compares the syntax violations the parser
// reports, in order.
func TestDiffAgainstRust_violations(t *testing.T) {
	withoutDebugAssertions(t)
	oracle.Check(t, "url", "url_violations", func(yield func(any)) {
		fixed := []string{
			"http://mozilla.org/\\foo", " http://mozilla.org", "http://user:pass@mozilla.org", "http:///mozilla.org",
			"file:/foo.txt", "file://mozilla.org/c:/file.txt", "http://mozilla.org/^", "http://mozilla.org/#\x000",
			"http://mozilla.org/%1", "http://mozilla.org\t/foo", "http://user@:pass@mozilla.org", "http:////mozilla.org:42",
			"http://mozilla.org\\path", "file:\\\\x\\y", "file:\\x", "http://h/%zz?%zz#%zz", "http://h/a%2", "sc:a b",
			"sc:%", "http://h/?\u00a0#\ufdd0", "http://a@b@c/", "\thttp://h/\n", "http:\\\\h", "http:/h", "http:h",
		}
		for _, input := range fixed {
			yield(map[string]any{"op": "violations", "input": input})
		}
		g := newGenerator(8)
		for range 1200 {
			yield(map[string]any{"op": "violations", "input": g.url()})
		}
		for range 600 {
			yield(map[string]any{"op": "violations", "input": g.pick(genReferences), "base": g.pick(genBases)})
		}
	}, answer)
}

// TestDiffAgainstRust_format compares the Debug and Display forms of Url,
// Host and ParseError under width and precision, and the two halves of the
// serde impls.
func TestDiffAgainstRust_format(t *testing.T) {
	withoutDebugAssertions(t)
	oracle.Check(t, "url", "url_format", func(yield func(any)) {
		fixed := []string{
			"http://user:pw@[::ffff:1.2.3.4]:8080/p?q#f", "http://127.0.0.1/", "bad", "mailto:x@y", "http://[1:0:0:2::3]/",
			"http://[::]/", "http://[::1]/", "http://[1::]/", "http://[1:2:3:4:5:6:7:8]/", "http://[::1.2.3.4]/",
			"http://[0:0:0:0:0:ffff:0:1]/", "http://[1:0:0:2:0:0:0:3]/", "http://[1:0:2:0:3:0:4:0]/", "http://[ffff::1:2]/",
			"http://\u00fcser:p\u00e4ss@b\u00fccher.example/\u00fc?\u00fc#\u00fc", "file:///C:/x", "data:text/plain,Hello?World#",
			"sc://\u00f1/x", "http://example.com:65535", "http://h/\"quoted\"\\", "sc:\\x\u007f", "http://[::1", "http://a:b@",
			"http://h:99999", "\u00a7invalid#+#*\u00c4", "", "http://0x7f.1/", "javascript:alert(1)",
		}
		for _, input := range fixed {
			yield(map[string]any{"op": "debug", "input": input})
			yield(map[string]any{"op": "serde", "input": input})
		}
		g := newGenerator(10)
		for range 300 {
			yield(map[string]any{"op": "debug", "input": g.url()})
		}
		for range 100 {
			yield(map[string]any{"op": "serde", "input": g.url()})
		}
	}, answer)
}

// TestDiffAgainstRust_misc compares make_relative, parse_with_params and the
// query encoding override.
func TestDiffAgainstRust_misc(t *testing.T) {
	withoutDebugAssertions(t)
	oracle.Check(t, "url", "url_misc", func(yield func(any)) {
		g := newGenerator(9)
		urls := append([]string{}, genBases...)
		urls = append(urls,
			"http://127.0.0.1:8080/test", "http://127.0.0.1:8080/test/", "http://127.0.0.1:8080/", "http://127.0.0.1:8080/?foo=bar#123",
			"http://127.0.0.1:8080/test/video", "http://127.0.0.1:8080/test2/video", "http://127.0.0.1:8080/test/bla/",
			"https://127.0.0.1:8080/test/", "http://127.0.0.1:8081/test/", "http://127.0.0.2:8080/test/", "http://example.com/a/b/c/d",
			"http://example.com/a/x/y", "http://example.com/a/b", "http://example.com/a/b/", "http://example.com//a//b", "http://example.com/",
			"http://user@example.com/a", "sc://host/a/b/c", "sc://host/a/x", "sc://other/a", "file:///a/b/c", "file:///a/x",
		)
		for _, base := range urls {
			for _, url := range urls {
				yield(map[string]any{"op": "make_relative", "base": base, "url": url})
			}
		}
		for range 300 {
			yield(map[string]any{"op": "make_relative", "base": g.url(), "url": g.url()})
		}
		for range 300 {
			params := [][]string{}
			for range g.random.IntN(4) {
				params = append(params, []string{g.pick(genPairParts), g.pick(genPairParts)})
			}
			input := g.pick(genBases)
			if g.chance(40) {
				input = g.url()
			}
			yield(map[string]any{"op": "parse_with_params", "input": input, "params": params, "brief": true})
		}
		for range 400 {
			yield(map[string]any{"op": "encoding_override", "input": g.pick(genSchemes) + "://h/p" + g.pick(genQueries) + g.pick(genFragments), "brief": true})
		}
	}, answer)
}
