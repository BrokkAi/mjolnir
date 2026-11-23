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
//
// It implements the parts of tokenizers 0.22 those files use: the added
// vocabulary, the NFC and Replace normalizers, the ByteLevel and Metaspace
// pre-tokenizers, the BPE model (with byte fallback and unknown-token
// fusing) and TemplateProcessing. A file naming any other component is
// refused when it is loaded, so a changed pin fails visibly instead of
// tokenizing differently.

package githubrelevance

import (
	"bytes"
	"container/heap"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"slices"
	"strings"
	"unicode"
	"unicode/utf8"

	"golang.org/x/text/unicode/norm"
)

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

// TruncationError's messages (tokenizers::utils::truncation).
var (
	errHFSecondSequenceNotProvided = errors.New("Truncation error: Second sequence not provided")
	errHFSequenceTooShort          = errors.New("Truncation error: Sequence to truncate too short to respect the provided max_length")
)

// loadHFTokenizer loads the tokenizer.json at path without padding,
// truncating as truncation says, or not at all when it is nil.
func loadHFTokenizer(path string, truncation *hfTruncation) (hfTokenizer, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	return parseHFTokenizer(data, truncation)
}

// hfTokenizerFile is the part of tokenizer.json this tokenizer reads. The
// file's own truncation and padding are replaced by the caller's, as Rust
// replaces them with with_truncation and with_padding.
type hfTokenizerFile struct {
	AddedTokens   []hfAddedToken  `json:"added_tokens"`
	Normalizer    json.RawMessage `json:"normalizer"`
	PreTokenizer  json.RawMessage `json:"pre_tokenizer"`
	PostProcessor json.RawMessage `json:"post_processor"`
	Model         json.RawMessage `json:"model"`
}

// hfAddedToken is tokenizers' AddedToken, as tokenizer.json lists it. The
// listed id is not used: ids are assigned as AddedVocabulary::add_tokens
// assigns them.
type hfAddedToken struct {
	Content    string `json:"content"`
	SingleWord bool   `json:"single_word"`
	Lstrip     bool   `json:"lstrip"`
	Rstrip     bool   `json:"rstrip"`
	Normalized bool   `json:"normalized"`
	Special    bool   `json:"special"`
}

type hfComponent struct {
	Type string `json:"type"`
}

// hfBPETokenizer is a TokenizerImpl with a BPE model.
type hfBPETokenizer struct {
	normalizer   hfNormalizer
	preTokenizer hfPreTokenizer
	model        *hfBPE
	// addedIDs is AddedVocabulary's added_tokens_map.
	addedIDs map[string]uint32
	// rawTokens and normalizedTokens are the split and normalized-split
	// tries: the tokens matched in the input as written, and those matched
	// in its normalized pieces (their content normalized too).
	rawTokens, normalizedTokens hfAddedMatcher
	post                        *hfTemplateProcessing
	truncation                  *hfTruncation
}

func parseHFTokenizer(data []byte, truncation *hfTruncation) (*hfBPETokenizer, error) {
	var file hfTokenizerFile
	if err := json.Unmarshal(data, &file); err != nil {
		return nil, err
	}
	normalizer, err := parseHFNormalizer(file.Normalizer)
	if err != nil {
		return nil, err
	}
	preTokenizer, err := parseHFPreTokenizer(file.PreTokenizer)
	if err != nil {
		return nil, err
	}
	model, err := parseHFBPE(file.Model)
	if err != nil {
		return nil, err
	}
	post, err := parseHFPostProcessor(file.PostProcessor)
	if err != nil {
		return nil, err
	}
	tokenizer := &hfBPETokenizer{
		normalizer:   normalizer,
		preTokenizer: preTokenizer,
		model:        model,
		addedIDs:     map[string]uint32{},
		post:         post,
		truncation:   truncation,
	}
	tokenizer.addTokens(file.AddedTokens)
	return tokenizer, nil
}

// addTokens is AddedVocabulary::add_tokens followed by
// refresh_added_tokens, on a new vocabulary.
func (t *hfBPETokenizer) addTokens(tokens []hfAddedToken) {
	var special, added []hfAddedToken
	specialSet := map[string]bool{}
	for _, token := range tokens {
		if token.Special && token.Content != "" && !specialSet[token.Content] {
			special = append(special, token)
			specialSet[token.Content] = true
		}
	}
	existing := map[hfAddedToken]bool{}
	vocabSize := uint32(len(t.model.vocab))
	nextID := vocabSize
	for _, token := range tokens {
		if token.Content == "" || existing[token] {
			continue
		}
		id, ok := t.tokenToID(token.Content)
		if !ok {
			id = nextID
			nextID++
		}
		t.addedIDs[token.Content] = id
		if !specialSet[token.Content] {
			added = append(added, token)
		}
		existing[token] = true
	}
	var raw, normalized []hfMatchToken
	for _, token := range append(special, added...) {
		id, _ := t.tokenToID(token.Content)
		if token.Normalized {
			content := token.Content
			if t.normalizer != nil {
				content = t.normalizer.normalize(content)
			}
			normalized = append(normalized, hfMatchToken{hfAddedToken: token, pattern: content, id: id})
		} else {
			raw = append(raw, hfMatchToken{hfAddedToken: token, pattern: token.Content, id: id})
		}
	}
	t.rawTokens = newHFAddedMatcher(raw)
	t.normalizedTokens = newHFAddedMatcher(normalized)
}

// tokenToID is AddedVocabulary::token_to_id: an added token's id, else the
// model's.
func (t *hfBPETokenizer) tokenToID(token string) (uint32, bool) {
	if id, ok := t.addedIDs[token]; ok {
		return id, true
	}
	id, ok := t.model.vocab[token]
	return id, ok
}

func (t *hfBPETokenizer) encode(first string, second *string, addSpecialTokens bool) (hfEncoding, error) {
	firstIDs, err := t.encodeSequence(first)
	if err != nil {
		return hfEncoding{}, err
	}
	var secondIDs []uint32
	if second != nil {
		if secondIDs, err = t.encodeSequence(*second); err != nil {
			return hfEncoding{}, err
		}
	}
	if t.truncation != nil {
		maxLength := t.truncation.maxLength
		if added := t.addedTokens(second != nil); addSpecialTokens && added > 0 {
			maxLength -= added
		}
		if firstIDs, secondIDs, err = truncateOnlySecond(firstIDs, secondIDs, second != nil, maxLength); err != nil {
			return hfEncoding{}, err
		}
	}
	var ids []uint32
	if addSpecialTokens && t.post != nil {
		template := t.post.single
		if second != nil {
			template = t.post.pair
		}
		for _, piece := range template {
			switch piece.sequence {
			case "A":
				ids = append(ids, firstIDs...)
			case "B":
				ids = append(ids, secondIDs...)
			default:
				ids = append(ids, piece.ids...)
			}
		}
	} else {
		ids = append(firstIDs, secondIDs...)
	}
	mask := make([]uint32, len(ids))
	for i := range mask {
		mask[i] = 1
	}
	return hfEncoding{ids: ids, attentionMask: mask}, nil
}

// addedTokens is PostProcessor::added_tokens: how many special tokens the
// template adds.
func (t *hfBPETokenizer) addedTokens(isPair bool) int {
	if t.post == nil {
		return 0
	}
	template := t.post.single
	if isPair {
		template = t.post.pair
	}
	count := 0
	for _, piece := range template {
		if piece.sequence == "" {
			count += len(piece.ids)
		}
	}
	return count
}

// truncateOnlySecond is truncate_encodings with the OnlySecond strategy
// and the Right direction.
func truncateOnlySecond(first, second []uint32, pair bool, maxLength int) ([]uint32, []uint32, error) {
	if maxLength == 0 {
		return first[:0], second[:0], nil
	}
	total := len(first) + len(second)
	if total <= maxLength {
		return first, second, nil
	}
	if !pair {
		return nil, nil, errHFSecondSequenceNotProvided
	}
	toRemove := total - maxLength
	if len(second) <= toRemove {
		return nil, nil, errHFSequenceTooShort
	}
	return first, second[:len(second)-toRemove], nil
}

// hfPiece is one split of a PreTokenizedString: text still to tokenize, or
// an added token already matched. atStart says the piece begins at offset
// 0 of the original input (Metaspace's First scheme).
type hfPiece struct {
	text    string
	token   *hfMatchToken
	atStart bool
}

// encodeSequence is encode_single_sequence: added tokens written verbatim
// are split out of the raw text, the rest is normalized and split on the
// normalized added tokens, pre-tokenized, and each word goes through the
// model.
func (t *hfBPETokenizer) encodeSequence(sequence string) ([]uint32, error) {
	var ids []uint32
	for _, raw := range t.rawTokens.split(sequence, true) {
		if raw.token != nil {
			ids = append(ids, raw.token.id)
			continue
		}
		text := raw.text
		if t.normalizer != nil {
			text = t.normalizer.normalize(text)
		}
		for _, piece := range t.normalizedTokens.split(text, raw.atStart) {
			if piece.token != nil {
				ids = append(ids, piece.token.id)
				continue
			}
			words := []string{piece.text}
			if t.preTokenizer != nil {
				words = t.preTokenizer.preTokenize(piece.text, piece.atStart)
			}
			for _, word := range words {
				if word == "" {
					continue
				}
				wordIDs, err := t.model.tokenize(word)
				if err != nil {
					return nil, err
				}
				ids = append(ids, wordIDs...)
			}
		}
	}
	return ids, nil
}

// hfMatchToken is an added token as a trie pattern.
type hfMatchToken struct {
	hfAddedToken
	pattern string
	id      uint32
}

// hfAddedMatcher finds added tokens leftmost-longest, as the
// AhoCorasick tries with MatchKind::LeftmostLongest do. byFirst holds the
// patterns by first byte, longest first, earlier patterns first among equal
// lengths.
type hfAddedMatcher struct {
	byFirst map[byte][]*hfMatchToken
}

func newHFAddedMatcher(tokens []hfMatchToken) hfAddedMatcher {
	matcher := hfAddedMatcher{byFirst: map[byte][]*hfMatchToken{}}
	for i := range tokens {
		if tokens[i].pattern == "" {
			continue
		}
		first := tokens[i].pattern[0]
		matcher.byFirst[first] = append(matcher.byFirst[first], &tokens[i])
	}
	for _, candidates := range matcher.byFirst {
		slices.SortStableFunc(candidates, func(a, b *hfMatchToken) int { return len(b.pattern) - len(a.pattern) })
	}
	return matcher
}

// split is AddedVocabulary::split_with_indices over find_matches: the
// matched tokens, with lstrip, rstrip and single_word applied, and the text
// between them as unmatched pieces. Empty unmatched pieces are dropped, as
// PreTokenizedString::split drops them. atStart says sentence begins the
// original input.
func (m hfAddedMatcher) split(sentence string, atStart bool) []hfPiece {
	if sentence == "" {
		return nil
	}
	var pieces []hfPiece
	push := func(start, stop int, token *hfMatchToken) {
		if start == stop && token == nil {
			return
		}
		pieces = append(pieces, hfPiece{text: sentence[start:stop], token: token, atStart: atStart && start == 0})
	}
	startOffset := 0
	for position := 0; position < len(sentence); {
		var match *hfMatchToken
		for _, candidate := range m.byFirst[sentence[position]] {
			if strings.HasPrefix(sentence[position:], candidate.pattern) {
				match = candidate
				break
			}
		}
		if match == nil {
			position++
			continue
		}
		start, stop := position, position+len(match.pattern)
		position = stop
		if match.SingleWord {
			startSpace := start == 0 || !hfEndsWithWord(sentence[:start])
			stopSpace := stop == len(sentence) || !hfStartsWithWord(sentence[stop:])
			if !stopSpace || !startSpace {
				continue
			}
		}
		if match.Lstrip {
			start = max(len(strings.TrimRightFunc(sentence[:start], unicode.IsSpace)), startOffset)
		}
		if match.Rstrip {
			stop += len(sentence[stop:]) - len(strings.TrimLeftFunc(sentence[stop:], unicode.IsSpace))
		}
		push(startOffset, start, nil)
		push(start, stop, match)
		startOffset = stop
	}
	push(startOffset, len(sentence), nil)
	return pieces
}

// hfIsWord is regex's Unicode \w.
func hfIsWord(c rune) bool {
	return c == '_' || unicode.IsLetter(c) || unicode.IsDigit(c) || unicode.Is(unicode.Mn, c) ||
		unicode.Is(unicode.Mc, c) || unicode.Is(unicode.Me, c) || unicode.Is(unicode.Nd, c) ||
		unicode.Is(unicode.Pc, c) || unicode.Is(unicode.Join_Control, c)
}

func hfEndsWithWord(text string) bool {
	c, _ := utf8.DecodeLastRuneInString(text)
	return text != "" && hfIsWord(c)
}

func hfStartsWithWord(text string) bool {
	c, _ := utf8.DecodeRuneInString(text)
	return text != "" && hfIsWord(c)
}

// hfNormalizer is a NormalizerWrapper.
type hfNormalizer interface {
	normalize(text string) string
}

type hfNFC struct{}

func (hfNFC) normalize(text string) string { return norm.NFC.String(text) }

type hfNFD struct{}

func (hfNFD) normalize(text string) string { return norm.NFD.String(text) }

type hfNFKC struct{}

func (hfNFKC) normalize(text string) string { return norm.NFKC.String(text) }

type hfNFKD struct{}

func (hfNFKD) normalize(text string) string { return norm.NFKD.String(text) }

// hfReplace is the Replace normalizer with a String pattern.
type hfReplace struct {
	pattern, content string
}

func (r hfReplace) normalize(text string) string {
	if r.pattern == "" {
		return text
	}
	return strings.ReplaceAll(text, r.pattern, r.content)
}

type hfNormalizerSequence []hfNormalizer

func (s hfNormalizerSequence) normalize(text string) string {
	for _, normalizer := range s {
		text = normalizer.normalize(text)
	}
	return text
}

func parseHFNormalizer(data json.RawMessage) (hfNormalizer, error) {
	if isJSONNull(data) {
		return nil, nil
	}
	var component hfComponent
	if err := json.Unmarshal(data, &component); err != nil {
		return nil, err
	}
	switch component.Type {
	case "NFC":
		return hfNFC{}, nil
	case "NFD":
		return hfNFD{}, nil
	case "NFKC":
		return hfNFKC{}, nil
	case "NFKD":
		return hfNFKD{}, nil
	case "Replace":
		var replace struct {
			Pattern struct {
				String *string `json:"String"`
			} `json:"pattern"`
			Content string `json:"content"`
		}
		if err := json.Unmarshal(data, &replace); err != nil {
			return nil, err
		}
		if replace.Pattern.String == nil {
			return nil, errors.New("unsupported tokenizer normalizer: Replace with a regex pattern")
		}
		return hfReplace{pattern: *replace.Pattern.String, content: replace.Content}, nil
	case "Sequence":
		var sequence struct {
			Normalizers []json.RawMessage `json:"normalizers"`
		}
		if err := json.Unmarshal(data, &sequence); err != nil {
			return nil, err
		}
		var parsed hfNormalizerSequence
		for _, item := range sequence.Normalizers {
			normalizer, err := parseHFNormalizer(item)
			if err != nil {
				return nil, err
			}
			if normalizer != nil {
				parsed = append(parsed, normalizer)
			}
		}
		return parsed, nil
	}
	return nil, fmt.Errorf("unsupported tokenizer normalizer: %s", component.Type)
}

// hfPreTokenizer is a PreTokenizerWrapper over one unmatched piece: the
// words the model tokenizes. atStart says the piece begins the original
// input.
type hfPreTokenizer interface {
	preTokenize(text string, atStart bool) []string
}

func parseHFPreTokenizer(data json.RawMessage) (hfPreTokenizer, error) {
	if isJSONNull(data) {
		return nil, nil
	}
	var component hfComponent
	if err := json.Unmarshal(data, &component); err != nil {
		return nil, err
	}
	switch component.Type {
	case "ByteLevel":
		byteLevel := hfByteLevel{addPrefixSpace: true, useRegex: true}
		var fields struct {
			AddPrefixSpace *bool `json:"add_prefix_space"`
			UseRegex       *bool `json:"use_regex"`
		}
		if err := json.Unmarshal(data, &fields); err != nil {
			return nil, err
		}
		if fields.AddPrefixSpace != nil {
			byteLevel.addPrefixSpace = *fields.AddPrefixSpace
		}
		if fields.UseRegex != nil {
			byteLevel.useRegex = *fields.UseRegex
		}
		return byteLevel, nil
	case "Metaspace":
		var fields struct {
			Replacement    string  `json:"replacement"`
			AddPrefixSpace *bool   `json:"add_prefix_space"`
			PrependScheme  *string `json:"prepend_scheme"`
			Split          *bool   `json:"split"`
		}
		if err := json.Unmarshal(data, &fields); err != nil {
			return nil, err
		}
		if utf8.RuneCountInString(fields.Replacement) != 1 {
			return nil, fmt.Errorf("invalid Metaspace replacement %q", fields.Replacement)
		}
		metaspace := hfMetaspace{scheme: "always", split: true}
		metaspace.replacement, _ = utf8.DecodeRuneInString(fields.Replacement)
		if fields.PrependScheme != nil {
			switch *fields.PrependScheme {
			case "first", "never", "always":
				metaspace.scheme = *fields.PrependScheme
			default:
				return nil, fmt.Errorf("unknown variant `%s`, expected one of `first`, `never`, `always`", *fields.PrependScheme)
			}
		}
		if fields.AddPrefixSpace != nil && !*fields.AddPrefixSpace {
			if metaspace.scheme != "never" {
				return nil, errors.New("add_prefix_space does not match declared prepend_scheme")
			}
		}
		if fields.Split != nil {
			metaspace.split = *fields.Split
		}
		return metaspace, nil
	}
	return nil, fmt.Errorf("unsupported tokenizer pre-tokenizer: %s", component.Type)
}

// hfByteLevel is the ByteLevel pre-tokenizer: the GPT-2 split, then each
// word's bytes as their printable characters.
type hfByteLevel struct {
	addPrefixSpace bool
	useRegex       bool
}

func (b hfByteLevel) preTokenize(text string, _ bool) []string {
	if b.addPrefixSpace && !strings.HasPrefix(text, " ") {
		text = " " + text
	}
	words := []string{text}
	if b.useRegex {
		words = hfByteLevelWords(text)
	}
	mapped := make([]string, 0, len(words))
	for _, word := range words {
		var builder strings.Builder
		for i := 0; i < len(word); i++ {
			builder.WriteRune(hfBytesChar[word[i]])
		}
		mapped = append(mapped, builder.String())
	}
	return mapped
}

// hfBytesChar is BYTES_CHAR, GPT-2's bytes_to_unicode: printable bytes map
// to themselves, the others to code points from 256 on.
var hfBytesChar = func() [256]rune {
	var table [256]rune
	printable := func(b int) bool {
		return (b >= '!' && b <= '~') || (b >= 0xA1 && b <= 0xAC) || (b >= 0xAE && b <= 0xFF)
	}
	next := rune(256)
	for b := 0; b < 256; b++ {
		if printable(b) {
			table[b] = rune(b)
		} else {
			table[b] = next
			next++
		}
	}
	return table
}()

// hfByteLevelWords splits text as ByteLevel's use_regex does, by the GPT-2
// expression
// `'s|'t|'re|'ve|'m|'ll|'d| ?\p{L}+| ?\p{N}+| ?[^\s\p{L}\p{N}]+|\s+(?!\S)|\s+`
// with Unicode `\s` (Oniguruma's). Go's regexp has no lookahead, so the
// alternatives are tried here in order at each position, as the
// leftmost-first expression does; every character is matched, so the
// Isolated split leaves no gaps.
func hfByteLevelWords(text string) []string {
	isSpace := unicode.IsSpace
	isLetter := unicode.IsLetter
	isNumber := unicode.IsNumber
	isOther := func(c rune) bool { return !isSpace(c) && !isLetter(c) && !isNumber(c) }
	run := func(i int, class func(rune) bool) int {
		for i < len(text) {
			c, size := utf8.DecodeRuneInString(text[i:])
			if !class(c) {
				break
			}
			i += size
		}
		return i
	}
	var words []string
	for i := 0; i < len(text); {
		end := i
		rest := text[i:]
		for _, contraction := range []string{"'s", "'t", "'re", "'ve", "'m", "'ll", "'d"} {
			if strings.HasPrefix(rest, contraction) {
				end = i + len(contraction)
				break
			}
		}
		if end == i {
			for _, class := range []func(rune) bool{isLetter, isNumber, isOther} {
				start := i
				if text[start] == ' ' && start+1 < len(text) {
					if c, _ := utf8.DecodeRuneInString(text[start+1:]); class(c) {
						start++
					}
				}
				if c, _ := utf8.DecodeRuneInString(text[start:]); class(c) {
					end = run(start, class)
					break
				}
			}
		}
		if end == i {
			// Whitespace: the run, less its last character when a
			// non-space follows, or that one character alone.
			stop := run(i, isSpace)
			if stop < len(text) {
				_, last := utf8.DecodeLastRuneInString(text[i:stop])
				if stop-last > i {
					stop -= last
				}
			}
			end = stop
		}
		words = append(words, text[i:end])
		i = end
	}
	return words
}

// hfMetaspace is the Metaspace pre-tokenizer: spaces become the
// replacement, which is prepended per the scheme, and the text splits
// before each run of replacements (MergedWithNext).
type hfMetaspace struct {
	replacement rune
	scheme      string
	split       bool
}

func (m hfMetaspace) preTokenize(text string, atStart bool) []string {
	replacement := string(m.replacement)
	text = strings.ReplaceAll(text, " ", replacement)
	// NormalizedString::prepend does nothing to an empty string.
	if text != "" && !strings.HasPrefix(text, replacement) {
		if m.scheme == "always" || (m.scheme == "first" && atStart) {
			text = replacement + text
		}
	}
	if !m.split {
		return []string{text}
	}
	// Pattern::find_matches for a char: each replacement alone, and the
	// text between them.
	type span struct {
		start, end int
		match      bool
	}
	var spans []span
	gap := 0
	for i, c := range text {
		if c != m.replacement {
			continue
		}
		if gap < i {
			spans = append(spans, span{start: gap, end: i})
		}
		gap = i + len(replacement)
		spans = append(spans, span{start: i, end: gap, match: true})
	}
	if gap < len(text) {
		spans = append(spans, span{start: gap, end: len(text)})
	}
	// MergedWithNext folds from the end: the last replacement of a run
	// joins the text after it; the others, and a trailing one, stand alone.
	var merged []span
	previousMatch := false
	for i := len(spans) - 1; i >= 0; i-- {
		current := spans[i]
		if current.match && !previousMatch && len(merged) > 0 {
			merged[len(merged)-1].start = current.start
		} else {
			merged = append(merged, current)
		}
		previousMatch = current.match
	}
	words := make([]string, 0, len(merged))
	for i := len(merged) - 1; i >= 0; i-- {
		words = append(words, text[merged[i].start:merged[i].end])
	}
	return words
}

// hfTemplatePiece is one element of a TemplateProcessing template: a
// special token's ids, or the first ("A") or second ("B") sequence.
type hfTemplatePiece struct {
	ids      []uint32
	sequence string
}

type hfTemplateProcessing struct {
	single, pair []hfTemplatePiece
}

func parseHFPostProcessor(data json.RawMessage) (*hfTemplateProcessing, error) {
	if isJSONNull(data) {
		return nil, nil
	}
	var definition struct {
		Type          string                            `json:"type"`
		Single        []map[string]struct{ ID string }  `json:"single"`
		Pair          []map[string]struct{ ID string }  `json:"pair"`
		SpecialTokens map[string]struct{ IDs []uint32 } `json:"special_tokens"`
	}
	if err := json.Unmarshal(data, &definition); err != nil {
		return nil, err
	}
	if definition.Type != "TemplateProcessing" {
		return nil, fmt.Errorf("unsupported tokenizer post-processor: %s", definition.Type)
	}
	template := func(pieces []map[string]struct{ ID string }) ([]hfTemplatePiece, error) {
		var parsed []hfTemplatePiece
		for _, piece := range pieces {
			if sequence, ok := piece["Sequence"]; ok {
				if sequence.ID != "A" && sequence.ID != "B" {
					return nil, fmt.Errorf("unknown template sequence %q", sequence.ID)
				}
				parsed = append(parsed, hfTemplatePiece{sequence: sequence.ID})
				continue
			}
			special, ok := definition.SpecialTokens[piece["SpecialToken"].ID]
			if !ok {
				return nil, fmt.Errorf("Missing SpecialToken(s) with id(s) `%s`", piece["SpecialToken"].ID)
			}
			parsed = append(parsed, hfTemplatePiece{ids: special.IDs})
		}
		return parsed, nil
	}
	single, err := template(definition.Single)
	if err != nil {
		return nil, err
	}
	pair, err := template(definition.Pair)
	if err != nil {
		return nil, err
	}
	return &hfTemplateProcessing{single: single, pair: pair}, nil
}

// hfBPE is the BPE model without dropout.
type hfBPE struct {
	vocab map[string]uint32
	// merges is the MergeMap: a pair's rank and the id it merges into.
	merges                  map[[2]uint32]hfMergeTarget
	unkToken                *string
	continuingSubwordPrefix *string
	endOfWordSuffix         *string
	fuseUnk                 bool
	byteFallback            bool
	ignoreMerges            bool
}

type hfMergeTarget struct {
	rank  int
	newID uint32
}

func parseHFBPE(data json.RawMessage) (*hfBPE, error) {
	var definition struct {
		Type                    string            `json:"type"`
		Dropout                 *float64          `json:"dropout"`
		UnkToken                *string           `json:"unk_token"`
		ContinuingSubwordPrefix *string           `json:"continuing_subword_prefix"`
		EndOfWordSuffix         *string           `json:"end_of_word_suffix"`
		FuseUnk                 bool              `json:"fuse_unk"`
		ByteFallback            bool              `json:"byte_fallback"`
		IgnoreMerges            bool              `json:"ignore_merges"`
		Vocab                   map[string]uint32 `json:"vocab"`
		Merges                  json.RawMessage   `json:"merges"`
	}
	if err := json.Unmarshal(data, &definition); err != nil {
		return nil, err
	}
	if definition.Type != "BPE" {
		return nil, fmt.Errorf("unsupported tokenizer model: %s", definition.Type)
	}
	if definition.Dropout != nil && *definition.Dropout != 0 {
		return nil, errors.New("unsupported tokenizer model: BPE with dropout")
	}
	merges, err := parseHFMerges(definition.Merges)
	if err != nil {
		return nil, err
	}
	model := &hfBPE{
		vocab:                   definition.Vocab,
		merges:                  make(map[[2]uint32]hfMergeTarget, len(merges)),
		unkToken:                definition.UnkToken,
		continuingSubwordPrefix: definition.ContinuingSubwordPrefix,
		endOfWordSuffix:         definition.EndOfWordSuffix,
		fuseUnk:                 definition.FuseUnk,
		byteFallback:            definition.ByteFallback,
		ignoreMerges:            definition.IgnoreMerges,
	}
	if model.vocab == nil {
		model.vocab = map[string]uint32{}
	}
	prefixLength := 0
	if model.continuingSubwordPrefix != nil {
		prefixLength = len(*model.continuingSubwordPrefix)
	}
	for rank, merge := range merges {
		a, ok := model.vocab[merge[0]]
		if !ok {
			return nil, fmt.Errorf("Token `%s` out of vocabulary", merge[0])
		}
		b, ok := model.vocab[merge[1]]
		if !ok {
			return nil, fmt.Errorf("Token `%s` out of vocabulary", merge[1])
		}
		if len(merge[1]) < prefixLength {
			return nil, fmt.Errorf("Token `%s` out of vocabulary", merge[1])
		}
		merged := merge[0] + merge[1][prefixLength:]
		newID, ok := model.vocab[merged]
		if !ok {
			return nil, fmt.Errorf("Token `%s` out of vocabulary", merged)
		}
		// A repeated pair keeps its last rank, as collecting into a map
		// does.
		model.merges[[2]uint32{a, b}] = hfMergeTarget{rank: rank, newID: newID}
	}
	return model, nil
}

// parseHFMerges reads the merges as pairs, or as the legacy "a b" lines.
func parseHFMerges(data json.RawMessage) ([][2]string, error) {
	if isJSONNull(data) {
		return nil, nil
	}
	var pairs [][2]string
	if err := json.Unmarshal(data, &pairs); err == nil {
		return pairs, nil
	}
	var lines []string
	if err := json.Unmarshal(data, &lines); err != nil {
		return nil, errors.New("data did not match any variant of untagged enum MergeType")
	}
	pairs = make([][2]string, 0, len(lines))
	rank := 0
	for _, line := range lines {
		if strings.HasPrefix(line, "#version") {
			continue
		}
		parts := strings.Split(line, " ")
		if len(parts) != 2 {
			return nil, fmt.Errorf("Merges text file invalid at line %d", rank+1)
		}
		pairs = append(pairs, [2]string{parts[0], parts[1]})
		rank++
	}
	return pairs, nil
}

// tokenize is BPE::tokenize without dropout.
func (m *hfBPE) tokenize(sequence string) ([]uint32, error) {
	if sequence == "" {
		return nil, nil
	}
	if m.ignoreMerges {
		if id, ok := m.vocab[sequence]; ok {
			return []uint32{id}, nil
		}
	}
	symbols, err := m.mergeWord(sequence)
	if err != nil {
		return nil, err
	}
	ids := make([]uint32, 0, len(symbols))
	for _, symbol := range symbols {
		if symbol.len != 0 {
			ids = append(ids, symbol.id)
		}
	}
	return ids, nil
}

// hfSymbol is one symbol of a word being merged, linked to its neighbours
// (bpe::word::Symbol).
type hfSymbol struct {
	id         uint32
	prev, next int
	len        int
}

// mergeWord is BPE::merge_word followed by Word::merge_all: the word's
// characters as vocabulary ids (byte fallback tokens, or the unknown token,
// for characters the vocabulary lacks), merged lowest rank first and
// leftmost first among equal ranks. Merged-away symbols keep len 0.
func (m *hfBPE) mergeWord(word string) ([]hfSymbol, error) {
	symbols := make([]hfSymbol, 0, len(word))
	add := func(id uint32, byteLength int) {
		symbol := hfSymbol{id: id, prev: -1, next: -1, len: byteLength}
		if count := len(symbols); count > 0 {
			symbols[count-1].next = count
			symbol.prev = count - 1
		}
		symbols = append(symbols, symbol)
	}
	var unk *hfSymbol
	for i := 0; i < len(word); {
		_, size := utf8.DecodeRuneInString(word[i:])
		s := word[i : i+size]
		start := i
		i += size
		byteLength := len(s)
		if start != 0 && m.continuingSubwordPrefix != nil {
			s = *m.continuingSubwordPrefix + s
		}
		if i == len(word) && m.endOfWordSuffix != nil {
			s += *m.endOfWordSuffix
		}
		if id, ok := m.vocab[s]; ok {
			if unk != nil {
				add(unk.id, unk.len)
				unk = nil
			}
			add(id, byteLength)
			continue
		}
		if m.byteFallback {
			fallback := make([]uint32, 0, len(s))
			for j := 0; j < len(s); j++ {
				id, ok := m.vocab[fmt.Sprintf("<0x%02X>", s[j])]
				if !ok {
					fallback = nil
					break
				}
				fallback = append(fallback, id)
			}
			if fallback != nil {
				for _, id := range fallback {
					add(id, 1)
				}
				continue
			}
		}
		if m.unkToken != nil {
			unkID, ok := m.vocab[*m.unkToken]
			switch {
			case unk != nil && m.fuseUnk:
				unk.len += byteLength
			case unk != nil:
				add(unk.id, unk.len)
				if !ok {
					return nil, fmt.Errorf("Unk token `%s` not found in the vocabulary", *m.unkToken)
				}
				unk = &hfSymbol{id: unkID, len: byteLength}
			default:
				if !ok {
					return nil, fmt.Errorf("Unk token `%s` not found in the vocabulary", *m.unkToken)
				}
				unk = &hfSymbol{id: unkID, len: byteLength}
			}
		}
	}
	if unk != nil {
		add(unk.id, unk.len)
	}
	m.mergeAll(symbols)
	return symbols, nil
}

// hfMerge is a pending merge of the symbol at pos with its next one.
type hfMerge struct {
	pos   int
	rank  int
	newID uint32
}

// hfMergeQueue is a min-heap by rank, then by position, as Word's
// BinaryHeap of merges orders them.
type hfMergeQueue []hfMerge

func (q hfMergeQueue) Len() int { return len(q) }
func (q hfMergeQueue) Less(i, j int) bool {
	if q[i].rank != q[j].rank {
		return q[i].rank < q[j].rank
	}
	return q[i].pos < q[j].pos
}
func (q hfMergeQueue) Swap(i, j int) { q[i], q[j] = q[j], q[i] }
func (q *hfMergeQueue) Push(x any)   { *q = append(*q, x.(hfMerge)) }
func (q *hfMergeQueue) Pop() any {
	old := *q
	last := old[len(old)-1]
	*q = old[:len(old)-1]
	return last
}

// mergeAll is Word::merge_all without dropout.
func (m *hfBPE) mergeAll(symbols []hfSymbol) {
	queue := make(hfMergeQueue, 0, len(symbols))
	for index := 0; index+1 < len(symbols); index++ {
		if target, ok := m.merges[[2]uint32{symbols[index].id, symbols[index+1].id}]; ok {
			queue = append(queue, hfMerge{pos: index, rank: target.rank, newID: target.newID})
		}
	}
	heap.Init(&queue)
	for queue.Len() > 0 {
		top := heap.Pop(&queue).(hfMerge)
		if symbols[top.pos].len == 0 || symbols[top.pos].next == -1 {
			continue
		}
		nextPos := symbols[top.pos].next
		right := symbols[nextPos]
		// Skip an expired queue entry.
		target, ok := m.merges[[2]uint32{symbols[top.pos].id, right.id}]
		if !ok || target.newID != top.newID {
			continue
		}
		symbols[top.pos].id = top.newID
		symbols[top.pos].len += right.len
		symbols[top.pos].next = right.next
		symbols[nextPos].len = 0
		if right.next > -1 && right.next < len(symbols) {
			symbols[right.next].prev = top.pos
		}
		current := symbols[top.pos]
		if current.prev >= 0 {
			if target, ok := m.merges[[2]uint32{symbols[current.prev].id, current.id}]; ok {
				heap.Push(&queue, hfMerge{pos: current.prev, rank: target.rank, newID: target.newID})
			}
		}
		if next := current.next; next > -1 && next < len(symbols) {
			if target, ok := m.merges[[2]uint32{current.id, symbols[next].id}]; ok {
				heap.Push(&queue, hfMerge{pos: top.pos, rank: target.rank, newID: target.newID})
			}
		}
	}
}

func isJSONNull(data json.RawMessage) bool {
	trimmed := bytes.TrimSpace(data)
	return len(trimmed) == 0 || bytes.Equal(trimmed, []byte("null"))
}
