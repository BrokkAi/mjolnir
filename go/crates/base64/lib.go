// Port of base64 0.22.1 src/lib.rs

package base64

// Correct, fast, and configurable [base64][] decoding and encoding. Base64
// transports binary data efficiently in contexts where only plain text is
// allowed.
//
// [base64]: https://developer.mozilla.org/en-US/docs/Glossary/Base64
//
// Use an [`Engine`] to decode or encode base64, configured with the base64
// alphabet and padding behavior best suited to your application. The module
// documentation of the crate is summarised in doc.go.

const libPadByte byte = '='
