// Package base64 stands in for the Rust crate `base64` 0.22.1, built as
// Mjolnir builds it: with the default `std` (and `alloc`) features.
//
// Mjolnir encodes and decodes with two of the crate's engines:
// `general_purpose::STANDARD` for clipboard and attachment images
// (mj-chat), daemon reply fragments (mj-client/src/daemon/chunk_bytes.rs)
// and checkpoint data, and `general_purpose::URL_SAFE_NO_PAD` for attachment
// references (mj-core/src/attachment.rs), JSON Web Tokens
// (mj-core/src/credentials.rs, mj-session-runtime/src/github_app.rs,
// mj-store/src/web_push.rs) and authentication tokens
// (mj-server/src/server/auth.rs). A decode error reaches the user through
// anyhow context or a serde error, as its Display text.
//
// The package catalogue lists base64 as a wrapper, but Go's encoding/base64
// accepts and rejects differently (it skips newlines, reports other offsets,
// and accepts non-canonical trailing bits unless Strict) and none of its
// errors carry the crate's variant, offset and symbol. Which error the crate
// reports, and at which offset, follows from the order of its decode loop,
// so the `general_purpose` engine is ported from the crate's source.
//
// # Names
//
//	base64::engine::general_purpose::STANDARD   base64.EngineGeneralPurposeStandard
//	  ::STANDARD_NO_PAD, ::URL_SAFE              base64.EngineGeneralPurposeStandardNoPad, ...UrlSafe
//	  ::URL_SAFE_NO_PAD                          base64.EngineGeneralPurposeUrlSafeNoPad
//	STANDARD.encode(bytes)                      base64.EngineGeneralPurposeStandard.Encode(bytes)
//	STANDARD.decode(text)                       base64.EngineGeneralPurposeStandard.Decode([]byte(text))
//	base64::Engine (the trait)                  base64.Engine
//	base64::DecodeError::InvalidByte(i, b)      *base64.DecodeError_InvalidByte{F0: i, F1: b}
//	base64::DecodeSliceError, EncodeSliceError  base64.DecodeSliceError, base64.EncodeSliceError
//	base64::engine::GeneralPurpose::new(a, c)   base64.EngineGeneralPurposeNew(&a, c)
//	base64::engine::GeneralPurposeConfig::new() base64.EngineGeneralPurposeConfigNew()
//	base64::engine::general_purpose::PAD        base64.EngineGeneralPurposePad (and NoPad)
//	base64::engine::DecodePaddingMode::RequireNone  base64.EngineDecodePaddingMode_RequireNone
//	base64::alphabet::STANDARD, Alphabet::new   base64.AlphabetStandard, base64.AlphabetAlphabetNew
//	base64::encoded_len, decoded_len_estimate   base64.EncodedLen, base64.DecodedLenEstimate
//
// The Engine methods take Rust's `T: AsRef<[u8]>` input as a []byte; a Rust
// caller that passes a `&str` becomes `[]byte(s)`. Errors are returned as Go
// errors holding the crate's error enum: Decode, DecodeVec and
// DecodeSliceUnchecked return a DecodeError, DecodeSlice a DecodeSliceError,
// EncodeSlice an EncodeSliceError. Match the variant with a type switch.
// Their Error text is the crate's Display text, and the formatter prints
// their Debug form (`InvalidByte(3, 45)`) from the registration.
//
// A usize is a Go int. EncodedLen therefore reports None once the length no
// longer fits in an int (from inputs of about 2^63 / 4 * 3 bytes) rather
// than in a u64; no real input comes near either bound.
//
// # Covered
//
//   - src/engine/general_purpose/ (mod.rs, decode.rs, decode_suffix.rs):
//     the GeneralPurpose engine, its configuration and the four engines
//     and two configurations the crate defines.
//   - src/engine/mod.rs: the Engine trait with every method (encode,
//     encode_string, encode_slice, decode, decode_vec, decode_slice,
//     decode_slice_unchecked), Config, DecodeEstimate, DecodePaddingMode
//     and DecodeMetadata.
//   - src/decode.rs, src/encode.rs: DecodeError, DecodeSliceError,
//     EncodeSliceError, encoded_len and decoded_len_estimate.
//   - src/alphabet.rs: Alphabet, its validation and ParseAlphabetError, and
//     the six alphabets.
//   - src/chunked_encoder.rs, which encode_string uses.
//
// # Not covered
//
//   - src/display.rs (Base64Display), src/read/ (DecoderReader) and
//     src/write/ (EncoderWriter, EncoderStringWriter): Mjolnir uses none of
//     them.
//   - src/prelude.rs (the BASE64_* aliases) and the deprecated free
//     functions (encode, decode, encode_engine, decode_engine, ...), which
//     forward to the Engine methods.
//   - Tests of the above, and the cases of src/engine/tests.rs that run on
//     the crate's naive test engine and on DecoderReader: each engine test
//     is ported for the GeneralPurpose engine.
//
// # Behaviour to know about
//
//   - STANDARD (and URL_SAFE) require canonical padding: "Zg" is
//     `Invalid padding`. STANDARD_NO_PAD and URL_SAFE_NO_PAD reject any
//     padding: "Zg==" is `Invalid padding` too.
//   - A last symbol whose unused bits are set is rejected: "/x==" is
//     `Invalid last symbol 120, offset 1.` (the symbol is printed as its
//     byte value).
//   - Anything outside the alphabet, white space and a stray `=` included,
//     is `Invalid symbol <byte>, offset <index>.`; a newline after
//     otherwise valid padded input is reported at its own offset. The URL-
//     safe and standard alphabets reject each other's `-`, `_`, `+` and `/`.
//   - A last group of one symbol (with no padding) is `Invalid input
//     length: <length of the input>`.
//   - When the output slice of decode_slice is too small for the complete
//     groups, that is reported before any invalid symbol in them.
//
// # Tests
//
// The crate's unit tests for what is covered are ported (alphabet_test.go,
// chunked_encoder_test.go, decode_test.go, encode_test.go, engine_test.go,
// engine_general_purpose_decode_test.go, lib_test.go) with the integration
// tests (it_tests_test.go, it_encode_test.go); their random inputs come
// from fixed seeds. oracle_test.go compares every Engine method with the
// real crate, for the four engines and custom engines over every alphabet
// and padding mode, on written-out inputs, valid encodings broken in one
// place, unstructured bytes (not only UTF-8), output buffers of every size
// around the needed one, alphabets and lengths.
package base64
