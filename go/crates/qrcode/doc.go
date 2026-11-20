// Package qrcode stands in for the Rust crate qrcode 0.14.1, built with
// `default-features = false` as mj-tui uses it. It is a port: types.rs,
// cast.rs, bits.rs, optimize.rs (the segment parser and optimizer), ec.rs
// (Reed-Solomon codes and interleaving), canvas.rs (function patterns, data
// placement, all eight masks and the penalty scoring that picks one), lib.rs,
// render/mod.rs and render/string.rs follow the crate line for line, so the
// same data gives the same module matrix.
//
// # Names
//
//	QrCode::with_error_correction_level(d, EcLevel::L)
//	                                    QrCodeWithErrorCorrectionLevel(d, EcLevel_L)
//	QrCode::new(d), QrCode::with_version(d, v, l), QrCode::with_bits(b, l)
//	                                    QrCodeNew, QrCodeWithVersion, QrCodeWithBits
//	qr.width(), qr[(x, y)]              qr.Width(), qr.Index(x, y)
//	qrcode::types::Color::Dark          Color_Dark (qrcode::Color)
//	qrcode::types::EcLevel::L           EcLevel_L (qrcode::EcLevel)
//	Version::Normal(5), Version::Micro(2)
//	                                    &Version_Normal{F0: 5}, &Version_Micro{F0: 2}
//	qrcode::types::QrError::DataTooLong TypesQrError_DataTooLong
//	qrcode::bits::Bits::new(v)          BitsBitsNew(v)
//	qrcode::canvas::Canvas::new(v, l)   CanvasCanvasNew(v, l)
//	qrcode::ec::construct_codewords     EcConstructCodewords
//	qrcode::optimize::Parser::new(d)    OptimizeParserNew(d)
//	qr.render::<char>()                 QrCodeRender[rune](qr)
//
// A QrResult<T> is `(T, error)`, the error a TypesQrError whose Display
// text is the crate's ("data too long", ...). Version, ExtendedMode and
// canvas::Module are enums with data, so their values are pointers to the
// variant structs; compare them with Equal.
//
// # Covered
//
// QrCode (new, with_error_correction_level, with_version, with_bits,
// version, error_correction_level, width, max_allowed_errors,
// is_functional, to_debug_str, to_vec, into_vec, to_colors, into_colors,
// render, Index), Color (select, Not), EcLevel, Version (width, fetch,
// mode_bits_count, is_micro), QrError, Mode (length_bits_count,
// data_bits_count, max, PartialOrd), bits::Bits with every push,
// bits::encode_auto, optimize::{Segment, Parser, Optimizer,
// total_encoded_len}, ec::{create_error_correction_code,
// construct_codewords, max_allowed_errors}, canvas::{Canvas, Module,
// MaskPattern, is_functional}, and render::Renderer for the string pixels
// `char` and `&str`.
//
// # Not covered
//
// The `image`, `svg` and `pic` renderers (features Mjolnir turns off) and the
// Dense1x2 pixel of render/unicode.rs, which nothing uses; the `bench`
// feature; the qrencode binary.
//
// # Arithmetic and debug assertions
//
// The crate's u8 and u16 arithmetic, its `As` conversions (cast.rs) and its
// debug assertions panic in a build with debug assertions and wrap or are
// skipped without them, as in Rust; DebugAssertions (the mj_debug build tag)
// decides which. Inputs from outside the crate's domain show the difference:
//
//   - A Micro QR code whose data ends inside its last, half-filled byte
//     (Version::Micro(3) at EcLevel::L or M with 18 to 21 digits, say) makes
//     the padding count in Bits.PushTerminator go below zero. Rust panics
//     with "attempt to subtract with overflow" with debug assertions and with
//     "capacity overflow" without (Vec::extend cannot reserve usize::MAX
//     bytes); so does this package. QrCodeWithErrorCorrectionLevel never
//     chooses a Micro version and cannot get there.
//   - Versions outside 1..=40 and 1..=4 are not checked up front by the
//     crate. Normal(0), Normal(41) and Micro(5) give InvalidVersion where the
//     crate does; Micro(0) and negative numbers are not compared with Rust:
//     without debug assertions Rust computes with usize::MAX where this
//     package's int is -1.
//
// # Tests
//
// The crate's own tests of every ported file (including the ISO Annex I
// symbols), and oracle_test.go, which compares the package with the Rust
// crate (go/port/oracle/src/qrcode_oracle.rs) on thousands of inputs:
// whole codes at every level, in automatically chosen and in every fixed
// version (module matrix, functional modules, renderings, errors and their
// texts), sequences of Bits pushes, segmentation, Reed-Solomon codewords,
// and each mask with the penalty scores' choice. Requests describe their
// data by a seeded generator both sides expand, so large inputs cost a few
// bytes of corpus; MJ_ORACLE=live adds the largest cases.
package qrcode
