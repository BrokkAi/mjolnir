// Port of mj-core/src/packed_text.rs.
//
// Immutable transcript text; compression never changes its serialized form.

package core

import "encoding/json"

// PackedText holds text either plain or as a zstd frame (level 3) when that
// is smaller. The zero value is empty plain text.
type PackedText struct {
	plain string
	// frame is the zstd frame of the text when compressed; nil for plain.
	frame []byte
	// length is the uncompressed byte length when frame is set.
	length int
}

// PackedTextPlain keeps an accumulating terminal plain until its final
// observation.
func PackedTextPlain(text string) PackedText {
	return PackedText{plain: text}
}

// NewPackedText compresses text when its zstd frame is smaller (Rust's
// From<String> and From<&str>).
func NewPackedText(text string) PackedText {
	panic("unported: mj-core/src/packed_text.rs From<String> for PackedText (needs a zstd codec)")
}

func (p PackedText) Text() string {
	if p.frame == nil {
		return p.plain
	}
	panic("unported: mj-core/src/packed_text.rs PackedText::text (needs a zstd codec)")
}

func (p PackedText) IntoText() string {
	return p.Text()
}

func (p PackedText) Len() int {
	if p.frame == nil {
		return len(p.plain)
	}
	return p.length
}

func (p PackedText) IsEmpty() bool {
	return p.Len() == 0
}

// Equal compares the texts, whatever their representation.
func (p PackedText) Equal(other PackedText) bool {
	return p.Text() == other.Text()
}

// MarshalJSON writes the text as a JSON string.
func (p PackedText) MarshalJSON() ([]byte, error) {
	return relayMarshalJSON(p.Text())
}

// UnmarshalJSON reads a JSON string and packs it.
func (p *PackedText) UnmarshalJSON(data []byte) error {
	var text string
	if err := json.Unmarshal(data, &text); err != nil {
		return err
	}
	*p = NewPackedText(text)
	return nil
}
