// Package url stands in for the Rust crate url 2.5.8, with the parts of its
// dependencies form_urlencoded 1.2.2, percent-encoding 2.3.2, idna 1.1.0
// and idna_adapter 1.2.2 that it uses. It is a port: the parser
// (parser.rs, host.rs, path_segments.rs, quirks.rs, origin.rs, slicing.rs
// and lib.rs) follows the crate line for line, so URLs parse, serialize,
// join and change exactly as they do in Rust, which is the WHATWG URL
// Standard and not Go's net/url.
//
// # Names
//
//	url::Url::parse(s)                 UrlParse(s)
//	"..".parse::<Url>(), Url::try_from UrlFromStr, UrlTryFromStr
//	Url::options().base_url(..).parse  UrlOptions().BaseUrl(..).Parse(..)
//	Url::parse_with_params             UrlParseWithParams
//	url.join(s), url.make_relative(&u) u.Join(s), u.MakeRelative(&v)
//	url.query_pairs()                  u.QueryPairs() (FormUrlencodedParseIter)
//	url.query_pairs_mut()              u.QueryPairsMut(), then Drop()
//	url.path_segments_mut()            u.PathSegmentsMut(), then Drop()
//	Url::from_file_path(p)             UrlFromFilePath(p)
//	&url[Position::A..Position::B]      u.IndexRange(Position_A, Position_B)
//	url::Host::Domain(d)               &Host_Domain{F0: d}
//	url::Host::Ipv4(a), Ipv6(a)        &Host_Ipv4{F0: a}, &Host_Ipv6{F0: a}
//	url::form_urlencoded::parse(b)     FormUrlencodedParse(b)
//	url::form_urlencoded::byte_serialize(b) FormUrlencodedByteSerialize(b)
//	form_urlencoded::Serializer::new(String::new())
//	                                   FormUrlencodedSerializerNew(FormUrlencodedTargetString(""))
//	url::quirks::f                     QuirksF
//
// A Url is a plain value: assigning one copies it, as `.clone()` does. The
// `Err(())` of the Rust methods that return `Result<(), ()>` is a non-nil
// error whose text is "()"; a ParseError is a ParseError value. Ipv4Addr and
// Ipv6Addr are netip.Addr (4 and 16 bytes); set_ip_host takes a netip.Addr,
// a 4-byte one standing for IpAddr::V4. The two serializers that borrow
// the URL (QueryPairsMut, PathSegmentsMut) hold its query and fragment
// aside, as the Rust guards do, and put them back on Drop.
//
// # Covered
//
//   - url::Url: parsing with every ParseOptions setting, join,
//     make_relative, parse_with_params, every getter (scheme, is_special,
//     has_authority, authority, cannot_be_a_base, username, password,
//     has_host, host_str, host, domain, port, port_or_known_default, path,
//     path_segments, query, query_pairs, fragment, origin), every setter,
//     path_segments_mut, query_pairs_mut, from_file_path,
//     from_directory_path and to_file_path in both their Unix and Windows
//     builds (through go/rs/rspath), check_invariants with Rust's message
//     text, indexing by Position, Display, Debug (also `{:#?}` and width and
//     precision), PartialEq, Ord, Hash (HashKey), FromStr, TryFrom<&str>,
//     From<Url> for String, AsRef<str>, and the two halves of the serde
//     impls (SerializeStr, UrlDeserializeStr and UrlDeserializeExpecting).
//   - url::Host (parse, parse_opaque, Display, Debug, Eq, Ord, Hash,
//     to_owned), url::Origin (new_opaque, is_tuple, ascii_serialization,
//     unicode_serialization, Eq, Hash), OpaqueOrigin, Position, ParseError
//     and SyntaxViolation with their exact Display and Debug texts,
//     url::quirks (every getter and setter, domain_to_ascii,
//     domain_to_unicode), PathSegmentsMut, UrlQuery.
//   - url::form_urlencoded: parse (Parse, ParseIntoOwned), byte_serialize
//     (ByteSerialize, yielding the same slices), Serializer with every
//     method, Target, EncodingOverride.
//   - percent-encoding and idna, privately, as far as url calls them.
//
// # Not covered
//
//   - Url::socket_addrs (resolves names; Mjolnir does not call it) and
//     quirks::internal_components (the `expose_internals` feature).
//   - The serde trait impls themselves: go/crates/serde did not build when
//     this package was finished, so there are no SerializeSerde and
//     DeserializeSerde methods yet. No Mjolnir type serializes a Url; when
//     one does, those methods call SerializeStr and UrlDeserializeStr.
//   - The public APIs of the percent-encoding and idna crates (AsciiSet,
//     percent_encode, idna::domain_to_ascii, idna::punycode, ...), which
//     Mjolnir does not use. Their items here are unexported and serve url.
//   - no_std, and the debugger visualizer.
//
// # IDNA
//
// The UTS 46 processing (domain_to_ascii for special hosts,
// domain_to_unicode for Origin::unicode_serialization and quirks) is the
// idna crate's own algorithm, ported line for line in idna.go and
// idna_punycode.go; golang.org/x/net/idna is not used for it. The Unicode
// data the algorithm reads comes from ICU4X 2.2.0 (Unicode 17.0) in Rust and,
// here, from idna_adapter.go: the UTS 46 mapping table of x/net/idna asked
// one character at a time, NFC and canonical combining classes of
// x/text/unicode/norm, Bidi_Class of x/text/unicode/bidi, General_Category of
// the unicode package, all Unicode 17.0.0 with Go 1.27 and the x/net and
// x/text versions in go.mod, and Joining_Type from idna_joining_table.go,
// which was read out of the Rust implementation (TestGenerateJoiningTable
// rebuilds it, TestDiffAgainstRust_joining_probe checks it).
//
// There is no known IDNA difference. Every source string of Unicode's IDNA
// conformance file (IdnaTestV2.txt, as idna 1.1.0 ships it) gives the same
// domain_to_ascii, domain_to_unicode and Host::parse result as Rust, and so
// does Host::parse of every Unicode scalar value alone and in three contexts
// (TestDiffAgainstRust_idna_conformance,
// TestDiffAgainstRust_host_every_scalar_value). A later Go, x/net or x/text
// moves the Go tables to a later Unicode version while the Rust crate stays
// on its pinned ICU4X data; those two tests then fail for the characters
// that changed.
//
// # Tests
//
// The crate's tests/unit.rs (it_unit_test.go; the tests Rust builds only for
// Unix or only for Windows run here on every system), the web-platform-tests
// data of tests/wpt.rs with urltestdata.json, setters_tests.json and
// expected_failures.txt copied into testdata/wpt (it_wpt_test.go, with the
// same 47 expected failures), the slicing.rs test, and the percent-encoding
// tests. Two Rust tests are skipped with their reason: `size`
// (Option<Url> niche layout) and `test_socket_addrs`.
//
// oracle_test.go compares the package with the Rust crate
// (go/port/oracle/src/url_oracle.rs) on generated URLs, relative
// references against bases of every kind, sequences of setter calls,
// hosts, IDNA, form_urlencoded on arbitrary bytes, file paths, syntax
// violations, make_relative, parse_with_params, the encoding override,
// Debug and Display forms, and serde. The comparisons run with debug
// assertions off, as the oracle is a release build.
package url
