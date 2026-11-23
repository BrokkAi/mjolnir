// Tokenizer for the downloaded models, in place of Rust's tokenizers crate.
//
// The student loads its model's tokenizer.json (ettin's, like the frozen
// history tokenizer) truncating the second segment of a pair to 7999
// tokens; the Guardian loads Wolf Defender's tokenizer.json without
// truncation, and looks up its special tokens. github.com/sugarme/tokenizer
// reads the files but tokenizes ByteLevel text differently from HuggingFace
// (see core's github_item.go, whose corrected encoder serves the history
// tokenizer), so this is a Mjolnir-local tokenizer producing HuggingFace's
// token IDs for these two files, as other local helpers stand in for
// library pieces elsewhere in the port.

package githubrelevance

// hfTokenizer is a loaded tokenizer.json with padding disabled (Rust's
// tokenizers::Tokenizer after with_padding(None)).
type hfTokenizer interface {
	// encode is Tokenizer::encode of first, or of the pair (first, second)
	// when second is not nil, with or without the post-processor's special
	// tokens, truncated as the tokenizer was loaded.
	encode(first string, second *string, addSpecialTokens bool) (hfEncoding, error)
	// tokenToID is Tokenizer::token_to_id.
	tokenToID(token string) (uint32, bool)
}

// hfEncoding is the part of tokenizers::Encoding the package reads.
type hfEncoding struct {
	ids           []uint32
	attentionMask []uint32
}

// hfTruncation is TruncationParams with the OnlySecond strategy, the only
// truncation the package uses: a pair longer than maxLength loses tokens
// from the end of its second segment, and is an error when the second
// segment is too short to lose enough.
type hfTruncation struct {
	maxLength int
}

// loadHFTokenizer loads the tokenizer.json at path without padding,
// truncating as truncation says, or not at all when it is nil.
func loadHFTokenizer(path string, truncation *hfTruncation) (hfTokenizer, error) {
	panic("unported: mj-github-relevance tokenizers::Tokenizer::from_file")
}
